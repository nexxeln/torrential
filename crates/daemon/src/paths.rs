use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use torrential_core::{Error, ErrorCode};

use crate::env::Env;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Paths {
    pub config: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
    /// What torrential keeps for the person, such as the plays it is downloading.
    pub data: PathBuf,
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
        let home = env.home().map(Path::to_path_buf);
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
                data: root.join("data"),
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
                data: xdg("XDG_DATA_HOME", ".local/share"),
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

    /// A new path for a player's socket that says where it is, beside the engine's.
    pub fn player_socket(&self) -> PathBuf {
        static PLAYERS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = PLAYERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.runtime.join(format!("player-{}-{n}.sock", std::process::id()))
    }

    pub fn lock(&self) -> PathBuf {
        self.state.join("engine.lock")
    }

    pub fn log(&self) -> PathBuf {
        self.state.join("engine.log")
    }

    /// The log before the last time it grew too long.
    pub fn previous_log(&self) -> PathBuf {
        self.state.join("engine.log.1")
    }

    pub fn database(&self) -> PathBuf {
        self.state.join("state.db")
    }

    /// torrential's own folder for plays. Each play downloads into a folder of its own inside it,
    /// and nothing else writes there.
    pub fn plays(&self) -> PathBuf {
        self.data.join("plays")
    }

    pub fn startup_error(&self) -> PathBuf {
        self.state.join("engine.error")
    }

    /// Keeps why the engine could not start, for the client that started it to report.
    pub fn record_startup_error(&self, error: &Error) {
        let file = self.startup_error();
        let written = std::fs::write(&file, serde_json::to_vec(error).expect("errors serialize"));
        crate::log::persist(&format!("write {}", file.display()), written);
    }

    /// Why the engine last could not start, or `None` when it left no record. A record that does not
    /// parse is an `Err`.
    pub fn recorded_startup_error(&self) -> Option<Result<Error, serde_json::Error>> {
        std::fs::read(self.startup_error()).ok().map(|bytes| serde_json::from_slice(&bytes))
    }

    /// Left by the engine for the next command to say it added the login service.
    pub fn login_service_added(&self) -> PathBuf {
        self.state.join("login-service-added")
    }

    /// Left by `torrential engine uninstall`, so the engine never adds the login service again.
    pub fn login_service_declined(&self) -> PathBuf {
        self.state.join("no-login-service")
    }

    /// Holds the port the engine last served the web app on.
    pub fn http_port(&self) -> PathBuf {
        self.state.join("http-port")
    }

    /// Left by an engine that exits for the login service to start the next one.
    pub fn switching(&self) -> PathBuf {
        self.state.join("engine.switching")
    }

    /// Makes the state folder, if it is not there, so that only this user can change it.
    pub fn make_state(&self) -> std::io::Result<()> {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.state)
    }
}

pub fn uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn no_home() -> Error {
    Error::new(ErrorCode::StateCorrupt, "HOME is not set, so torrential has no default location for its files")
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
        assert_eq!(p.plays(), Path::new("/tmp/th/data/plays"));
    }

    #[test]
    fn xdg_directories_apply_without_torrential_home() {
        let p = resolve(&[("HOME", "/Users/ada"), ("XDG_STATE_HOME", "/var/st")]).unwrap();
        assert_eq!(p.state, Path::new("/var/st/torrential"));
        assert_eq!(p.config, Path::new("/Users/ada/.config/torrential"));
        assert_eq!(p.plays(), Path::new("/Users/ada/.local/share/torrential/plays"));
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
