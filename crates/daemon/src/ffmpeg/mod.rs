//! Finds ffmpeg and ffprobe, or fetches them, for the web player to convert what a browser cannot
//! read.

use std::path::{Path, PathBuf};

pub mod fetch;

/// Where ffmpeg lives when the engine's own `PATH` is the bare one launchd and systemd give a service.
const USUAL_DIRS: [&str; 3] = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"];

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
    /// Looks in `path`, the value of `PATH`, then in the usual folders.
    pub fn find(path: Option<&str>) -> Tools {
        let dirs: Vec<&Path> = path.unwrap_or_default().split(':').chain(USUAL_DIRS).map(Path::new).collect();
        Tools::find_in(&dirs)
    }

    fn find_in(dirs: &[&Path]) -> Tools {
        let find = |name: &str| dirs.iter().map(|dir| dir.join(name)).find(|path| path.is_absolute() && path.is_file());
        Tools { ffmpeg: find("ffmpeg"), ffprobe: find("ffprobe") }
    }

    pub fn both(&self) -> Option<Programs> {
        Some(Programs { ffmpeg: self.ffmpeg.clone()?, ffprobe: self.ffprobe.clone()? })
    }
}

/// How a person installs ffmpeg here, as the end of a sentence.
pub const INSTALL: &str =
    if cfg!(target_os = "macos") { "run brew install ffmpeg" } else { "install ffmpeg with your package manager" };

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

    #[test]
    fn the_tools_are_found_in_the_first_folder_that_has_them() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        for (folder, name) in [(&first, "ffprobe"), (&second, "ffmpeg"), (&second, "ffprobe")] {
            std::fs::create_dir_all(folder).unwrap();
            std::fs::write(folder.join(name), "").unwrap();
        }
        let found = Tools::find_in(&[Path::new("relative"), &dir.path().join("none"), &first, &second]);
        assert_eq!(found, Tools { ffmpeg: Some(second.join("ffmpeg")), ffprobe: Some(first.join("ffprobe")) });
        assert!(found.both().is_some());
        assert_eq!(Tools::find_in(&[&first]).both(), None);
    }

    #[test]
    fn a_failure_reads_as_ffmpegs_first_line() {
        let stderr = "\n[matroska,webm @ 0x7a310a4300] EBML header parsing failed\nError opening input\n";
        assert_eq!(complaint(stderr), "EBML header parsing failed");
        assert_eq!(complaint("Error opening input file"), "Error opening input file");
        assert_eq!(complaint(" \n"), "it printed no reason");
    }
}
