use std::path::{Path, PathBuf};

use torrential_core::{Error, ErrorCode};

use crate::env::Env;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Paths {
    pub config: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
    pub runtime: PathBuf,
    pub downloads: PathBuf,
}

/// `sun_path` is 104 bytes on macOS and 108 on Linux, each including the final NUL.
#[cfg(target_os = "macos")]
pub const MAX_SOCKET_PATH: usize = 103;
#[cfg(not(target_os = "macos"))]
pub const MAX_SOCKET_PATH: usize = 107;

impl Paths {
    pub fn from_process(env: &Env) -> Result<Self, Error> {
        Self::resolve(env, uid(), dirs::download_dir())
    }

    pub fn resolve(env: &Env, uid: u32, os_downloads: Option<PathBuf>) -> Result<Self, Error> {
        let home = env.var("HOME").map(PathBuf::from);
        let torrential_home = env.var("TORRENTIAL_HOME").map(PathBuf::from);
        let downloads = os_downloads
            .or_else(|| home.as_ref().map(|h| h.join("Downloads")))
            .map(|d| d.join("torrential"))
            .or_else(|| torrential_home.as_ref().map(|r| r.join("downloads")))
            .ok_or_else(no_home)?;
        let paths = if let Some(root) = torrential_home {
            Self {
                config: root.join("config"),
                state: root.join("state"),
                cache: root.join("cache"),
                runtime: root.join("run"),
                downloads,
            }
        } else {
            let home = home.ok_or_else(no_home)?;
            let xdg = |var: &str, fallback: &str| {
                env.var(var).map_or_else(|| home.join(fallback), PathBuf::from).join("torrential")
            };
            Self {
                config: xdg("XDG_CONFIG_HOME", ".config"),
                state: xdg("XDG_STATE_HOME", ".local/state"),
                cache: xdg("XDG_CACHE_HOME", ".cache"),
                runtime: env.var("XDG_RUNTIME_DIR").map_or_else(
                    || PathBuf::from(format!("/tmp/torrential-{uid}")),
                    |d| PathBuf::from(d).join("torrential"),
                ),
                downloads,
            }
        };
        let socket = paths.socket();
        let len = socket.as_os_str().len();
        if len > MAX_SOCKET_PATH {
            return Err(Error::new(
                ErrorCode::DaemonUnavailable,
                format!(
                    "the engine socket path {} is {len} bytes, over the {MAX_SOCKET_PATH}-byte limit",
                    socket.display()
                ),
            )
            .with_hint("set TORRENTIAL_HOME to a shorter directory"));
        }
        Ok(paths)
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime.join("engine.sock")
    }

    pub fn lock(&self) -> PathBuf {
        self.state.join("engine.lock")
    }

    pub fn log(&self) -> PathBuf {
        self.state.join("engine.log")
    }

    pub fn database(&self) -> PathBuf {
        self.state.join("state.db")
    }

    pub fn startup_error(&self) -> PathBuf {
        self.state.join("engine.error")
    }
}

pub fn uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn no_home() -> Error {
    Error::new(ErrorCode::StateCorrupt, "HOME is not set, so torrential cannot place its state")
        .with_hint("set HOME, or set TORRENTIAL_HOME to a directory for torrential's files")
}

/// The filesystem type of `path` when it is a network filesystem, where SQLite's locking
/// and WAL are unreliable.
pub fn network_filesystem(path: &Path) -> std::io::Result<Option<String>> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(network_kind(&stat))
}

#[cfg(target_os = "macos")]
fn network_kind(stat: &libc::statfs) -> Option<String> {
    let name = unsafe { std::ffi::CStr::from_ptr(stat.f_fstypename.as_ptr()) }.to_string_lossy().into_owned();
    ["nfs", "smbfs", "afpfs", "webdav", "cifs", "ftp"].contains(&name.as_str()).then_some(name)
}

#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_cast)]
fn network_kind(stat: &libc::statfs) -> Option<String> {
    let kind = match stat.f_type as i64 {
        0x6969 => "nfs",
        0x517b => "smb",
        0xff53_4d42 => "cifs",
        0xfe53_4d42 => "smb2",
        0x0102_1997 => "9p",
        0x5346_414f => "afs",
        0x00c3_6400 => "ceph",
        0x6573_5546 => "fuse",
        _ => return None,
    };
    Some(kind.to_string())
}

pub fn check_state_filesystem(state: &Path) -> Result<(), Error> {
    match network_filesystem(state) {
        Ok(None) => Ok(()),
        Ok(Some(kind)) => Err(Error::new(
            ErrorCode::StateCorrupt,
            format!("the state directory {} is on a network filesystem ({kind})", state.display()),
        )
        .with_hint("set TORRENTIAL_HOME to a directory on a local disk")),
        Err(e) => Err(Error::new(ErrorCode::StateCorrupt, format!("cannot inspect {}: {e}", state.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(vars: &[(&str, &str)]) -> Result<Paths, Error> {
        Paths::resolve(&Env::from_pairs(vars.iter().copied()), 501, Some("/Users/ada/Downloads".into()))
    }

    #[test]
    fn torrential_home_holds_everything_but_downloads() {
        let p = resolve(&[("HOME", "/Users/ada"), ("TORRENTIAL_HOME", "/tmp/th")]).unwrap();
        assert_eq!(p.state, Path::new("/tmp/th/state"));
        assert_eq!(p.socket(), Path::new("/tmp/th/run/engine.sock"));
        assert_eq!(p.downloads, Path::new("/Users/ada/Downloads/torrential"));
    }

    #[test]
    fn xdg_directories_apply_without_torrential_home() {
        let p = resolve(&[("HOME", "/Users/ada"), ("XDG_STATE_HOME", "/var/st")]).unwrap();
        assert_eq!(p.state, Path::new("/var/st/torrential"));
        assert_eq!(p.config, Path::new("/Users/ada/.config/torrential"));
        assert_eq!(p.socket(), Path::new("/tmp/torrential-501/engine.sock"));
        let p = resolve(&[("HOME", "/home/ada"), ("XDG_RUNTIME_DIR", "/run/user/501")]).unwrap();
        assert_eq!(p.socket(), Path::new("/run/user/501/torrential/engine.sock"));
    }

    #[test]
    fn refuses_a_socket_path_past_the_limit() {
        let deep = format!("/tmp/{}", "d".repeat(120));
        let err = resolve(&[("HOME", "/Users/ada"), ("TORRENTIAL_HOME", &deep)]).unwrap_err();
        assert_eq!(err.code, ErrorCode::DaemonUnavailable);
        assert_eq!(err.hint.as_deref(), Some("set TORRENTIAL_HOME to a shorter directory"));
    }

    #[test]
    fn a_local_temp_dir_is_not_a_network_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(network_filesystem(dir.path()).unwrap(), None);
        assert!(check_state_filesystem(dir.path()).is_ok());
    }
}
