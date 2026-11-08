use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use serde::Serialize;
use torrential_core::protocol::{FilesReply, PlayReply, PlayRequest, Request, Response};
use torrential_core::stream::{self, Start};
use torrential_core::text::sanitize;
use torrential_core::{Category, Error, ErrorCode, Preferences, Profile, SearchQuery, Target, media};

use super::connect::{self, Conn};
use super::player::{self, Launch, Player};
use super::{absolute, answer, emit, file_list};
use crate::cli::PlayArgs;
use crate::config::{self, Config};
use crate::env::Env;
use crate::output::format::shell_word;
use crate::output::theme::{Token, paint};
use crate::output::{Doc, Mode, Typed, views};
use crate::paths::Paths;

/// Bytes read from each end of the video for its runtime: Matroska and fast-start MP4 keep it in
/// the head, and other MP4s in the tail.
const PROBE: u64 = 2 * 1024 * 1024;
const TICK: Duration = Duration::from_millis(500);
/// The countdown plans with the mean rate over this long, since the rate a moment reports swings as
/// pieces and peers come and go.
const RATE_WINDOW: Duration = Duration::from_secs(20);

#[derive(Serialize)]
pub struct Playing {
    pub name: String,
    #[serde(flatten)]
    pub reply: PlayReply,
}

/// What the countdown knows about how long the video runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Runtime {
    Reading,
    Known(Duration),
    Unknown,
}

/// The download rates seen over the last `RATE_WINDOW`.
#[derive(Default)]
struct Rates(VecDeque<(Instant, u64)>);

impl Rates {
    /// Adds the rate seen at `at` and returns the mean over the window that ends there.
    fn mean(&mut self, at: Instant, rate: u64) -> u64 {
        self.0.push_back((at, rate));
        while self.0.front().is_some_and(|&(seen, _)| at.duration_since(seen) > RATE_WINDOW) {
            self.0.pop_front();
        }
        self.0.iter().map(|&(_, rate)| rate).sum::<u64>() / self.0.len() as u64
    }
}

/// Why the player has not started yet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hold {
    /// The first bytes of the video are on their way.
    Reading,
    /// The download outruns playback, and the first seconds of video are on their way.
    Buffering(Duration),
    /// Playback would catch up with the download if it started before this.
    Countdown(Duration),
    Stalled,
}

pub fn run(args: PlayArgs, mode: Mode, env: &Env, cwd: &Path) -> Result<u8, Error> {
    let paths = Paths::from_process(env)?;
    let config = Config::load(&paths, env)?;
    let mut conn = connect::ensure(&paths, env)?;
    let finding = (mode == Mode::Human).then(|| {
        let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr())
            .with_style(ProgressStyle::with_template("{spinner} {msg}").expect("the template is valid"));
        bar.set_message("Finding the video and its file list");
        bar.enable_steady_tick(Duration::from_millis(100));
        bar
    });
    let found = resolve(&mut conn, &args.words, cwd, config.preferences);
    if let Some(bar) = finding {
        bar.finish_and_clear();
    }
    let (target, title, listing) = found?;
    let dir = args.dir.map(|d| absolute(&d, cwd, env));
    let reply = start(&mut conn, target, &listing, dir, config.preferences.max_size)?;
    let name = title.or(listing.name).unwrap_or_else(|| views::short_ref(&listing.id));
    let playing = Playing { name, reply };
    let doc = views::play(&playing.name, &playing.reply, None);
    if mode == Mode::Json {
        return Ok(emit(mode, &doc, &Typed { kind: "play", body: &playing }));
    }
    let player = player(&config, &paths, env, &playing.reply.streams[0].url)?;
    if mode == Mode::Plain {
        open(&player, &playing, env)?;
        let summary = vec![format!("playing in {} · the rest keeps downloading", player.name())];
        return Ok(emit(mode, &Doc { summary, ..doc }, &Typed { kind: "play", body: &playing }));
    }
    let id = views::short_ids(&[playing.reply.job.id]).remove(0);
    let _ = ctrlc::set_handler(move || {
        eprintln!("\nThe download keeps going: torrential status {id}");
        std::process::exit(130);
    });
    let runtime = countdown(conn, &playing)?;
    open(&player, &playing, env)?;
    let mut out = Doc { next: None, ..views::play(&playing.name, &playing.reply, known(runtime)) }.human();
    if runtime == Runtime::Unknown {
        out.push_str(&paint(Token::Muted, views::UNKNOWN_RUNTIME));
        out.push('\n');
    }
    out.push_str(&views::playing(&player.name(), &playing.reply.job));
    anstream::print!("{out}");
    Ok(0)
}

/// The player config.yaml names, or the first one installed. Without one, the error names `url`
/// for any other player.
pub fn player(config: &Config, paths: &Paths, env: &Env, url: &str) -> Result<Player, Error> {
    let choice = config.player.as_ref();
    match player::find(choice, env) {
        Some(player) => Ok(player),
        None => Err(player::missing(choice, url, &config::file(paths)?)),
    }
}

/// Opens the video and its subtitles in `player`, titled with the release name, showing subtitles in
/// the system language first.
pub fn open(player: &Player, playing: &Playing, env: &Env) -> Result<(), Error> {
    let [video, subtitles @ ..] = playing.reply.streams.as_slice() else {
        return Err(Error::internal("the engine streams no file to play"));
    };
    let language = crate::machine::system_language(env);
    player.launch(&Launch {
        video: &video.url,
        subtitles: subtitles.iter().map(|s| s.url.as_str()).collect(),
        language: language.as_ref().map(|l| l.code()),
        title: &sanitize(&playing.name),
    })
}

/// Picks the video and its subtitles from `listing` and asks the engine to stream them.
pub fn start(
    conn: &mut Conn,
    target: Target,
    listing: &FilesReply,
    dir: Option<PathBuf>,
    setup_max_size: Option<u64>,
) -> Result<PlayReply, Error> {
    let Some(playable) = stream::pick(&listing.files) else {
        let id = views::short_ref(&listing.id);
        let name = listing.name.clone().unwrap_or_else(|| id.clone());
        return Err(Error::new(ErrorCode::NoSelection, format!("{name} has no video file"))
            .with_hint(format!("run torrential files {id} to see what it holds"))
            .with_next(format!("torrential files {id}")));
    };
    let files = [playable.video].into_iter().chain(playable.subtitles).collect();
    match answer(conn.call(&Request::Play(PlayRequest { target, dir, files, setup_max_size }))?)? {
        Response::Play { reply } => Ok(*reply),
        other => Err(connect::unexpected(&other)),
    }
}

/// The torrent the words name, with its files and, when a search found it, the result's name. A
/// single word that names a download, a search result, or anything else `get` takes wins over a
/// search for the words, which takes the top video result.
fn resolve(
    conn: &mut Conn,
    words: &[String],
    cwd: &Path,
    preferences: Preferences,
) -> Result<(Target, Option<String>, FilesReply), Error> {
    if let [word] = words
        && let Ok(target) = Target::parse(word, cwd)
    {
        match file_list(conn, target.clone()) {
            Ok(listing) => return Ok((target, None, listing)),
            Err(error) if error.code == ErrorCode::NotFound && matches!(target, Target::Prefix(_)) => {}
            Err(error) => return Err(error),
        }
    }
    let text = words.join(" ");
    let query = SearchQuery {
        category: Some(Category::Video),
        profile: Profile { preferences, ..Profile::default() },
        ..SearchQuery::new(text.clone())
    };
    let within = Duration::from_millis(query.timeout_ms.into()) + Duration::from_secs(10);
    let releases = match answer(conn.call_within(&Request::Search { query, stream: false }, within)?)? {
        Response::Search { result } => result.releases,
        other => return Err(connect::unexpected(&other)),
    };
    let Some(release) = releases.into_iter().next() else {
        let search = format!("torrential search {}", words.iter().map(|w| shell_word(w)).collect::<Vec<_>>().join(" "));
        return Err(Error::new(ErrorCode::NotFound, format!("no video result matches {text:?}"))
            .with_hint(format!("run {search} to see what each source found"))
            .with_next(search));
    };
    let target = Target::Prefix(release.id.to_ref());
    let listing = file_list(conn, target.clone())?;
    Ok((target, Some(release.name), listing))
}

/// Why the player should wait, or `None` once it can start and never catch up with the download.
pub fn hold(runtime: Runtime, size: u64, have: u64, rate: u64) -> Option<Hold> {
    if have >= size {
        return None;
    }
    match runtime {
        Runtime::Unknown => None,
        Runtime::Reading if rate == 0 => Some(Hold::Stalled),
        Runtime::Reading => Some(Hold::Reading),
        Runtime::Known(runtime) => match stream::start(size, runtime, have, rate) {
            Start::Now => None,
            Start::Stalled => Some(Hold::Stalled),
            Start::Wait(wait) if rate as f64 * runtime.as_secs_f64() >= (size - have) as f64 => {
                Some(Hold::Buffering(wait))
            }
            Start::Wait(wait) => Some(Hold::Countdown(wait)),
        },
    }
}

fn known(runtime: Runtime) -> Option<Duration> {
    match runtime {
        Runtime::Known(runtime) => Some(runtime),
        Runtime::Reading | Runtime::Unknown => None,
    }
}

enum Wake {
    Runtime(Option<Duration>),
    Enter,
}

/// Shows why playback waits until it can start, or until the person presses Enter.
fn countdown(mut conn: Conn, playing: &Playing) -> Result<Runtime, Error> {
    let video = &playing.reply.streams[0];
    let (tx, rx) = mpsc::channel();
    let (probe, url) = (tx.clone(), video.url.clone());
    std::thread::spawn(move || {
        let _ = probe.send(Wake::Runtime(read_runtime(&url)));
    });
    std::thread::spawn(move || {
        let mut line = String::new();
        while std::io::stdin().read_line(&mut line).is_ok_and(|read| read > 0) {
            if tx.send(Wake::Enter).is_err() {
                return;
            }
        }
    });
    let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr())
        .with_style(ProgressStyle::with_template("{msg}").expect("the template is valid"));
    let mut runtime = Runtime::Reading;
    let mut rates = Rates::default();
    let id = playing.reply.job.id.to_hex();
    loop {
        let have = file_list(&mut conn, Target::Prefix(id.clone()))?
            .files
            .iter()
            .find(|f| f.index == video.index)
            .map_or(0, |f| f.verified);
        let rate = match answer(conn.call(&Request::Status { refs: vec![id.clone()] })?)? {
            Response::Jobs { jobs } => jobs.first().map_or(0, |job| job.progress.down_rate),
            other => return Err(connect::unexpected(&other)),
        };
        let rate = rates.mean(Instant::now(), rate);
        let Some(why) = hold(runtime, video.size, have, rate) else { break };
        let header = Doc { next: None, ..views::play(&playing.name, &playing.reply, known(runtime)) }.human();
        bar.set_message(format!("{header}{}", paint(Token::Accent, &views::hold(why))));
        match rx.recv_timeout(TICK) {
            Ok(Wake::Enter) => break,
            Ok(Wake::Runtime(read)) => runtime = read.map_or(Runtime::Unknown, Runtime::Known),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
    }
    bar.finish_and_clear();
    Ok(runtime)
}

/// Reads the runtime from the ends of the video. Asking for those bytes also makes the engine
/// fetch them first, and the player needs them before anything else.
fn read_runtime(url: &str) -> Option<Duration> {
    let head = fetch(url, &format!("bytes=0-{}", PROBE - 1)).ok()?;
    media::duration(&head, &[]).or_else(|| media::duration(&head, &fetch(url, &format!("bytes=-{PROBE}")).ok()?))
}

fn fetch(url: &str, range: &str) -> std::io::Result<Vec<u8>> {
    let url = url::Url::parse(url).map_err(std::io::Error::other)?;
    let (host, port) = (url.host_str().unwrap_or_default(), url.port_or_known_default().unwrap_or(80));
    let mut stream = TcpStream::connect((host, port))?;
    write!(
        stream,
        "GET {} HTTP/1.1\r\nHost: {host}:{port}\r\nRange: {range}\r\nConnection: close\r\n\r\n",
        url.path()
    )?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut head = httparse::Response::new(&mut headers);
    match head.parse(&response) {
        Ok(httparse::Status::Complete(length)) if matches!(head.code, Some(200 | 206)) => {
            Ok(response.split_off(length))
        }
        _ => Err(std::io::ErrorKind::InvalidData.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_with_the_mean_rate_of_the_last_20_seconds() {
        let start = Instant::now();
        let mut rates = Rates::default();
        let means = [(0, 900_000), (5, 300_000), (10, 600_000), (25, 1_200_000), (50, 0)]
            .map(|(second, rate)| rates.mean(start + Duration::from_secs(second), rate));
        assert_eq!(means, [900_000, 600_000, 600_000, 700_000, 0]);
    }

    #[test]
    fn holds_playback_until_it_cannot_catch_the_download() {
        let (size, runtime) = (5_000_000_000, Runtime::Known(Duration::from_secs(90 * 60)));
        let read = [
            hold(Runtime::Reading, size, 0, 0),
            hold(Runtime::Reading, size, 0, 3_000_000),
            hold(runtime, size, 0, 695_000),
            hold(runtime, size, 0, 50_000_000),
            hold(runtime, size, 10_000_000, 50_000_000),
            hold(runtime, size, 4_000_000_000, 0),
            hold(Runtime::Unknown, size, 0, 0),
            hold(Runtime::Reading, size, size, 0),
        ];
        assert_eq!(
            read,
            [
                Some(Hold::Stalled),
                Some(Hold::Reading),
                Some(Hold::Countdown(Duration::from_secs(2594))),
                Some(Hold::Buffering(Duration::from_secs(1))),
                None,
                Some(Hold::Stalled),
                None,
                None,
            ]
        );
    }
}
