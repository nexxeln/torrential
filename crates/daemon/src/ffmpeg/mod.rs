//! Finds ffmpeg and ffprobe, or fetches them, for the web player to convert what a browser cannot
//! read.

use std::io;
use std::path::{Path, PathBuf};

use torrential_core::{Error, ErrorCode};

use crate::env::Env;

pub mod fetch;

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Tools {
    pub ffmpeg: Option<PathBuf>,
    pub ffprobe: Option<PathBuf>,
}

/// ffmpeg and ffprobe, both found, which converting a file needs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Programs {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Tools {
    /// Looks in each folder of `env`'s `PATH`, then in the folders ffmpeg is usually installed to,
    /// which a `PATH` from before the install lacks. Each look reads the folders again, so an ffmpeg
    /// installed while the engine runs is found.
    pub fn find(env: &Env) -> Tools {
        let path = env.var("PATH").map(std::env::split_paths).into_iter().flatten();
        Tools::find_in(&path.chain(usual_dirs(env)).collect::<Vec<_>>())
    }

    fn find_in(dirs: &[PathBuf]) -> Tools {
        let find = |name: &str| {
            let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
            dirs.iter().map(|dir| dir.join(&file)).find(|path| path.is_absolute() && path.is_file())
        };
        Tools { ffmpeg: find("ffmpeg"), ffprobe: find("ffprobe") }
    }

    pub fn both(&self) -> Option<Programs> {
        Some(Programs { ffmpeg: self.ffmpeg.clone()?, ffprobe: self.ffprobe.clone()? })
    }
}

/// Where ffmpeg lives when the engine's own `PATH` is the bare one launchd and systemd give a service.
#[cfg(not(windows))]
fn usual_dirs(_: &Env) -> Vec<PathBuf> {
    ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from).into()
}

/// Where winget, Scoop and Chocolatey put ffmpeg. Each adds its folder to the person's `PATH`, which
/// an engine started before the install does not see.
#[cfg(windows)]
fn usual_dirs(env: &Env) -> Vec<PathBuf> {
    let under = |var: &str, rest: &str| env.var(var).map(|root| Path::new(root).join(rest));
    let winget = under("LOCALAPPDATA", r"Microsoft\WinGet");
    let links = [winget.as_ref().map(|winget| winget.join("Links")), under("ProgramFiles", r"WinGet\Links")];
    let shims = [under("USERPROFILE", r"scoop\shims"), under("ProgramData", r"chocolatey\bin")];
    let mut dirs: Vec<PathBuf> = links.into_iter().chain(shims).flatten().collect();
    dirs.extend(winget.iter().flat_map(|winget| gyan_bins(&winget.join("Packages"))));
    dirs
}

/// The `bin` folders of the Gyan ffmpeg builds winget unpacked into `packages`, as
/// `Gyan.FFmpeg_<source>\ffmpeg-<version>-full_build\bin`. winget links them into its `Links` folder
/// only when Windows lets it make links.
#[cfg(windows)]
fn gyan_bins(packages: &Path) -> Vec<PathBuf> {
    let folders = |dir: &Path| std::fs::read_dir(dir).into_iter().flatten().flatten().map(|entry| entry.path());
    let gyan = |path: &PathBuf| path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("Gyan.FFmpeg"));
    let mut bins: Vec<PathBuf> =
        folders(packages).filter(gyan).flat_map(|package| folders(&package).map(|build| build.join("bin"))).collect();
    bins.sort();
    bins
}

/// How a person installs ffmpeg here, as the end of a sentence.
pub const INSTALL: &str = if cfg!(target_os = "macos") {
    "run brew install ffmpeg"
} else if cfg!(windows) {
    "run winget install Gyan.FFmpeg in PowerShell"
} else {
    "install ffmpeg with your package manager"
};

/// The Windows errors for a program that Smart App Control, an App Control policy, or antivirus
/// will not start. Unix has no error numbers this high, so none of them is a Unix error.
const BLOCKED: [i32; 9] = [
    225,  // ERROR_VIRUS_INFECTED
    786,  // ERROR_ACCESS_DISABLED_NO_SAFER_UI_BY_POLICY
    1260, // ERROR_ACCESS_DISABLED_BY_POLICY
    4551, // ERROR_SYSTEM_INTEGRITY_POLICY_VIOLATION
    4556, // ERROR_SYSTEM_INTEGRITY_REPUTATION_MALICIOUS
    4557, // ERROR_SYSTEM_INTEGRITY_REPUTATION_PUA
    4558, // ERROR_SYSTEM_INTEGRITY_REPUTATION_DANGEROUS_EXT
    4580, // ERROR_SYSTEM_INTEGRITY_REPUTATION_UNFRIENDLY_FILE
    4582, // ERROR_SYSTEM_INTEGRITY_REPUTATION_EXPLICIT_DENY_FILE
];

/// Whether Windows refused to start a program because a policy forbids it, which trying again does
/// not change.
pub fn blocked(e: &io::Error) -> bool {
    e.raw_os_error().is_some_and(|code| BLOCKED.contains(&code))
}

/// What a person reads when Windows will not start `what`, a program named for them.
pub fn blocked_error(code: ErrorCode, what: &str) -> Error {
    Error::new(code, format!("Windows blocked {what} (Smart App Control or an App Control policy)"))
        .with_hint(format!("{INSTALL}, then press Play again"))
}

/// Why the installed or fetched `program` did not start.
pub fn cannot_run(program: &Path, e: &io::Error) -> Error {
    if blocked(e) {
        return blocked_error(ErrorCode::Internal, &program.display().to_string());
    }
    Error::internal(format!("cannot run {}: {e}", program.display()))
}

/// The first line ffmpeg printed about a failure, without the address it puts in its prefix.
pub fn complaint(stderr: &str) -> String {
    let line = stderr.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("it printed no reason");
    let line = match (line.starts_with('['), line.split_once("] ")) {
        (true, Some((_, rest))) => rest,
        _ => line,
    };
    torrential_core::text::sanitize(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Makes each of `files`, a folder and a program name, under `dir`, with `.exe` on Windows.
    fn programs(dir: &Path, files: &[(&str, &str)]) {
        for (folder, name) in files {
            let folder = dir.join(folder);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)), "").unwrap();
        }
    }

    fn exe(path: PathBuf) -> PathBuf {
        path.with_extension(std::env::consts::EXE_EXTENSION)
    }

    #[test]
    fn the_tools_are_found_in_the_first_folder_that_has_them() {
        let dir = tempfile::tempdir().unwrap();
        programs(dir.path(), &[("first", "ffprobe"), ("second", "ffmpeg"), ("second", "ffprobe")]);
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        let found =
            Tools::find_in(&[PathBuf::from("relative"), dir.path().join("none"), first.clone(), second.clone()]);
        let both = Tools { ffmpeg: Some(exe(second.join("ffmpeg"))), ffprobe: Some(exe(first.join("ffprobe"))) };
        assert_eq!(found, both);
        assert!(found.both().is_some());
        assert_eq!(Tools::find_in(&[first]).both(), None);
    }

    #[test]
    fn path_is_split_the_way_this_system_splits_it() {
        let dir = tempfile::tempdir().unwrap();
        programs(dir.path(), &[("a", "ffmpeg"), ("b", "ffprobe")]);
        let path = std::env::join_paths([dir.path().join("none"), dir.path().join("a"), dir.path().join("b")]).unwrap();
        let env = Env::from_pairs([("PATH", path.to_str().unwrap())]);
        let found = Tools::find(&env);
        assert_eq!(found.ffmpeg, Some(exe(dir.path().join("a").join("ffmpeg"))));
        assert_eq!(found.ffprobe, Some(exe(dir.path().join("b").join("ffprobe"))));
    }

    #[cfg(not(windows))]
    #[test]
    fn a_service_without_homebrew_on_its_path_still_looks_there() {
        let env = Env::from_pairs([("PATH", "/nowhere")]);
        assert_eq!(usual_dirs(&env), ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    }

    #[cfg(windows)]
    #[test]
    fn path_on_windows_is_split_on_semicolons_and_programs_end_in_exe() {
        let dir = tempfile::tempdir().unwrap();
        programs(dir.path(), &[("a b", "ffmpeg"), ("c", "ffprobe")]);
        let path = format!("{}\\none;{}\\a b;;{}\\c", dir.path().display(), dir.path().display(), dir.path().display());
        let found = Tools::find(&Env::from_pairs([("PATH", path.as_str())]));
        assert_eq!(found.ffmpeg, Some(dir.path().join("a b").join("ffmpeg.exe")));
        assert_eq!(found.ffprobe, Some(dir.path().join("c").join("ffprobe.exe")));
    }

    #[cfg(windows)]
    #[test]
    fn an_ffmpeg_a_package_manager_installed_after_the_engine_started_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let (local, profile, data) = (dir.path().join("Local"), dir.path().join("ada"), dir.path().join("ProgramData"));
        let env = Env::from_pairs([
            ("PATH", r"C:\nowhere"),
            ("LOCALAPPDATA", local.to_str().unwrap()),
            ("USERPROFILE", profile.to_str().unwrap()),
            ("ProgramData", data.to_str().unwrap()),
        ]);
        assert_eq!(Tools::find(&env), Tools::default());
        let package = r"Microsoft\WinGet\Packages\Gyan.FFmpeg_Microsoft.Winget.Source_8wekyb3d8bbwe";
        let winget = format!(r"{package}\ffmpeg-9.0.2-full_build\bin");
        programs(
            &local,
            &[(&winget, "ffmpeg"), (&winget, "ffprobe"), (r"Microsoft\WinGet\Packages\Other\x\bin", "ffmpeg")],
        );
        let found = Tools::find(&env);
        assert_eq!(found.ffmpeg, Some(local.join(&winget).join("ffmpeg.exe")));
        assert_eq!(found.ffprobe, Some(local.join(&winget).join("ffprobe.exe")));
        programs(&profile, &[(r"scoop\shims", "ffmpeg")]);
        programs(&data, &[(r"chocolatey\bin", "ffprobe")]);
        let found = Tools::find(&env);
        assert_eq!(found.ffmpeg, Some(profile.join(r"scoop\shims\ffmpeg.exe")));
        assert_eq!(found.ffprobe, Some(data.join(r"chocolatey\bin\ffprobe.exe")));
        programs(&local, &[(r"Microsoft\WinGet\Links", "ffmpeg")]);
        assert_eq!(Tools::find(&env).ffmpeg, Some(local.join(r"Microsoft\WinGet\Links\ffmpeg.exe")));
    }

    #[test]
    fn a_program_windows_blocks_says_so_and_how_to_get_one_it_runs() {
        let program = Path::new(r"C:\Tools\ffprobe.exe");
        for code in [4551, 1260, 225] {
            let error = cannot_run(program, &io::Error::from_raw_os_error(code));
            assert_eq!(
                error.message, r"Windows blocked C:\Tools\ffprobe.exe (Smart App Control or an App Control policy)",
                "{code}"
            );
            assert_eq!(error.hint.as_deref(), Some(format!("{INSTALL}, then press Play again").as_str()));
        }
        let missing = cannot_run(program, &io::Error::from_raw_os_error(2));
        assert!(missing.message.starts_with(r"cannot run C:\Tools\ffprobe.exe: "), "{}", missing.message);
        assert!(!blocked(&io::Error::from_raw_os_error(5)), "access denied is a permission, not a policy");
    }

    #[test]
    fn a_failure_reads_as_ffmpegs_first_line() {
        let stderr = "\n[matroska,webm @ 0x7a310a4300] EBML header parsing failed\nError opening input\n";
        assert_eq!(complaint(stderr), "EBML header parsing failed");
        assert_eq!(complaint("Error opening input file"), "Error opening input file");
        assert_eq!(complaint(" \n"), "it printed no reason");
    }
}
