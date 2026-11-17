use std::path::{Path, PathBuf};

use torrential_core::protocol::{
    Change, Event, FilesReply, Frame, GetReply, GetRequest, Home, Request, TitlePage, TitlePlayed, TitleRequest,
};
use torrential_core::{
    AttemptEvent, Error, Machine, Preferences, SearchQuery, SearchResult, Target, TitleCard, TitleId, VideoRef,
};

use torrential_tui::{ColorMode, Edit, Exit};

use super::connect::{self, Conn};
use super::play::{self, Playing};
use super::remote::{self, Remotes};
use super::titles;
use torrential_daemon::config::{self, Config};
use torrential_daemon::disk::{Disk, RealDisk};
use torrential_daemon::env::Env;
use torrential_daemon::fsutil::Writes;
use torrential_daemon::paths::Paths;
use torrential_daemon::update::release::{self, Outcome, Releases};

pub fn run(env: &Env) -> Result<u8, Error> {
    let paths = Paths::from_process(env)?;
    let downloads = config::downloads_dir(Config::load(&paths, env).as_ref(), &paths);
    let backend = EngineBackend { paths, downloads, env: env.clone() };
    let colors = ColorMode::detect(|key| env.var(key).map(String::from));
    let home = env.home().map(Path::to_path_buf);
    let exit = torrential_tui::run(backend, colors, home);
    match exit.map_err(|e| Error::internal(format!("the terminal UI stopped: {e}")))? {
        Exit::Quit => Ok(0),
        Exit::Restart => restart(&env.installed()?),
    }
}

/// Runs the torrential an update installed in place of this one.
#[cfg(unix)]
fn restart(exe: &Path) -> Result<u8, Error> {
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(exe).args(std::env::args_os().skip(1)).exec();
    Err(Error::internal(format!("cannot start the new torrential at {}: {error}", exe.display())))
}

/// Windows cannot run another program in this process's place, so the new torrential runs in
/// this terminal until it ends, and this one ends as it does.
#[cfg(windows)]
fn restart(exe: &Path) -> Result<u8, Error> {
    let status = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .status()
        .map_err(|e| Error::internal(format!("cannot start the new torrential at {}: {e}", exe.display())))?;
    Ok(status.code().and_then(|code| u8::try_from(code).ok()).unwrap_or(1))
}

pub(crate) struct EngineBackend {
    pub(crate) paths: Paths,
    pub(crate) downloads: PathBuf,
    pub(crate) env: Env,
}

impl EngineBackend {
    fn conn(&self) -> Result<Conn, Error> {
        connect::ensure(&self.paths, &self.env)
    }

    fn changes(&self, request: Request) -> Result<Vec<Change>, Error> {
        self.conn()?.ask(&request)
    }
}

impl torrential_tui::Backend for EngineBackend {
    fn search(
        &self,
        query: SearchQuery,
        partial: &mut dyn FnMut(SearchResult, Vec<String>),
    ) -> Result<SearchResult, Error> {
        self.conn()?.stream(&Request::Search { query, stream: true }, |update| {
            if let Event::SearchUpdate { result, pending } = update {
                partial(*result, pending);
            }
        })
    }

    fn files(&self, reference: &str) -> Result<FilesReply, Error> {
        super::file_list(&mut self.conn()?, Target::Prefix(reference.to_string()))
    }

    fn get(&self, reference: &str, files: Option<Vec<String>>) -> Result<GetReply, Error> {
        let request = Request::Get(GetRequest {
            target: Target::Prefix(reference.to_string()),
            dir: None,
            files: files.unwrap_or_default().iter().map(|path| globset::escape(path)).collect(),
            max_size: None,
            dry_run: false,
        });
        let reply: Box<GetReply> = self.conn()?.ask(&request)?;
        Ok(*reply)
    }

    fn play(&self, reference: &str) -> Result<String, Error> {
        remote::local_only(&self.paths, super::PLAYS_HERE)?;
        let config = Config::load(&self.paths, &self.env)?;
        let mut conn = self.conn()?;
        let target = Target::Prefix(reference.to_string());
        let listing = super::file_list(&mut conn, target.clone())?;
        let reply = play::start(&mut conn, target, &listing, None)?;
        let player = play::player(&config, &self.paths, &self.env, &reply.streams[0].url)?;
        let name = listing.name.unwrap_or_else(|| super::rows::short_ref(&listing.id));
        play::open(&mut conn, &player, &Playing { name, reply, title: None }, &self.env, &self.paths)?;
        Ok(player.name())
    }

    fn pause(&self, id: &str) -> Result<Vec<Change>, Error> {
        self.changes(Request::Pause { refs: vec![id.to_string()] })
    }

    fn resume(&self, id: &str) -> Result<Vec<Change>, Error> {
        self.changes(Request::Resume { refs: vec![id.to_string()], all: false })
    }

    fn remove(&self, id: &str) -> Result<Vec<Change>, Error> {
        self.changes(Request::Remove { refs: vec![id.to_string()], delete: false })
    }

    fn verify(&self, id: &str) -> Result<Vec<Change>, Error> {
        self.changes(Request::Verify { reference: id.to_string(), repair: false })
    }

    fn watch(&self, event: &mut dyn FnMut(Event)) -> Result<(), Error> {
        let mut conn = self.conn()?;
        conn.send(&Request::Watch { refs: Vec::new() })?;
        loop {
            match conn.next(None)? {
                Some(Frame::Update(update)) => event(update),
                Some(Frame::Reply(response)) => return connect::reply(*response),
                None => return Ok(()),
            }
        }
    }

    fn free_space(&self) -> Option<u64> {
        if Remotes::using(&self.paths).ok()?.is_some() {
            return None;
        }
        RealDisk.space(&self.downloads).ok().map(|space| space.free)
    }

    fn preferences(&self) -> Result<Preferences, Error> {
        config::preferences(&self.paths)
    }

    fn machine(&self) -> Option<Machine> {
        torrential_daemon::machine::detect(&self.env).ok()
    }

    fn save_preferences(&self, edit: Edit) -> Result<Preferences, Error> {
        remote::local_only(&self.paths, super::SETTINGS_HERE)?;
        let preferences = edit.apply(config::preferences(&self.paths)?);
        let writes = Writes::new(&self.env, "torrential setup".into());
        config::save_preferences(&self.paths, &preferences, &writes)?;
        Ok(preferences)
    }

    fn newer_version(&self) -> Option<String> {
        super::update::available(&self.paths, &self.env).map(|latest| latest.to_string())
    }

    fn upgrade(&self) -> Result<Option<String>, Error> {
        let releases = Releases::required(&self.env)?;
        let install = release::install(&releases, &self.paths, &self.env, torrential_core::time::now());
        Ok(match release::runtime()?.block_on(install)? {
            Outcome::Updated(latest) => Some(latest.to_string()),
            Outcome::Latest(_) => None,
        })
    }

    fn open(&self, folder: &Path) -> Result<(), Error> {
        let elsewhere = ("the download's folder is on the machine that runs the engine", "open it on that machine");
        remote::local_only(&self.paths, elsewhere)?;
        torrential_daemon::player::open_url(&folder.display().to_string(), &self.env)
    }

    fn home(&self) -> Result<Home, Error> {
        let home: Box<Home> = self.conn()?.ask(&Request::Home)?;
        Ok(*home)
    }

    fn titles(&self, words: &str) -> Result<Vec<TitleCard>, Error> {
        self.conn()?.ask(&Request::Titles { words: words.to_string() })
    }

    fn title(&self, id: &TitleId) -> Result<TitlePage, Error> {
        let title: Box<TitlePage> = self.conn()?.ask(&Request::Title { id: id.clone() })?;
        Ok(*title)
    }

    fn play_title(&self, video: &VideoRef, step: &mut dyn FnMut(AttemptEvent)) -> Result<(String, TitlePlayed), Error> {
        remote::local_only(&self.paths, super::PLAYS_HERE)?;
        let config = Config::load(&self.paths, &self.env)?;
        let mut conn = self.conn()?;
        let request = TitleRequest { video: video.clone(), source: None };
        let (reply, title) = titles::play_each(&mut conn, request, |event| step(event.clone()))?;
        let player = play::player(&config, &self.paths, &self.env, &reply.streams[0].url)?;
        let playing = Playing { name: title.name.clone(), reply, title: Some(title.clone()) };
        play::open(&mut conn, &player, &playing, &self.env, &self.paths)?;
        Ok((player.name(), title))
    }
}
