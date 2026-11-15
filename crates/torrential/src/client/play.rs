use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::playback::stream::{self, Start};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use serde::Serialize;
use torrential_core::{Job, Plays};
use torrential_core::protocol::{FilesReply, PlayReply, PlayRequest, Request, TitlePage, TitlePlayed, TitleRequest};
use torrential_core::text::sanitize;
use torrential_core::units::format_size;
use torrential_core::{Category, Error, ErrorCode, SearchQuery, SearchResult, Target, VideoRef};

use crate::playback::media;

use super::connect::{self, Conn};
use super::rows::{short_ids, short_ref};
use super::titles::view::resuming;
use super::{absolute, emit, file_list, titles};
use crate::cli::PlayArgs;
use crate::config::{self, Config};
use crate::env::Env;
use crate::output::format::{self, shell_word};
use crate::output::theme::{Token, paint};
use crate::output::{Body, Cell, Doc, DocKind, Mode, Typed};
use crate::paths::{MAX_SOCKET_PATH, Paths};
use crate::player::{self, Launch, Player};

const TICK: Duration = Duration::from_millis(500);
/// The countdown plans with the mean rate over this long, since the rate a moment reports swings as
/// pieces and peers come and go.
const RATE_WINDOW: Duration = Duration::from_secs(20);

#[derive(Serialize)]
pub struct Playing {
    pub name: String,
    #[serde(flatten)]
    pub reply: PlayReply,
    /// For a film or an episode, the source that plays and those that did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<TitlePlayed>,
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
    let video = titles::named(&args.words)?;
    match &video {
        Some(video) if args.dir.is_some() => return Err(torrential_core::target::not_a_torrent(video, "--dir")),
        None if args.source.is_some() => return Err(titles::for_a_title("--source")),
        _ => {}
    }
    let paths = Paths::from_process(env)?;
    let config = Config::load(&paths, env)?;
    let mut conn = connect::ensure(&paths, env)?;
    let playing = match video {
        Some(video) => {
            let (reply, title) = titles::play(&mut conn, TitleRequest { video, source: args.source }, mode)?;
            Playing { name: title.name.clone(), reply, title: Some(title) }
        }
        None => torrent(&mut conn, args, mode, env, cwd)?,
    };
    present(conn, playing, mode, env, &paths, &config)
}

/// Starts the video of the torrent `args` name, or of the top search result for its words.
fn torrent(conn: &mut Conn, args: PlayArgs, mode: Mode, env: &Env, cwd: &Path) -> Result<Playing, Error> {
    let finding = (mode == Mode::Human).then(|| {
        let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr())
            .with_style(ProgressStyle::with_template("{spinner} {msg}").expect("the template is valid"));
        bar.set_message("Finding the video and its file list");
        bar.enable_steady_tick(Duration::from_millis(100));
        bar
    });
    let found = resolve(conn, &args.words, cwd);
    if let Some(bar) = finding {
        bar.finish_and_clear();
    }
    let (target, title, listing) = found?;
    let dir = args.dir.map(|d| absolute(&d, cwd, env));
    let reply = start(conn, target, &listing, dir)?;
    let name = title.or(listing.name).unwrap_or_else(|| short_ref(&listing.id));
    Ok(Playing { name, reply, title: None })
}

/// Hands the video to the person's player, or with `--json` prints where it is served.
fn present(
    mut conn: Conn,
    mut playing: Playing,
    mode: Mode,
    env: &Env,
    paths: &Paths,
    config: &Config,
) -> Result<u8, Error> {
    let doc = play(&playing, None);
    if mode == Mode::Json {
        return Ok(emit(mode, &doc, &Typed { kind: DocKind::Play, body: &playing }));
    }
    let player = player(config, paths, env, &playing.reply.streams[0].url)?;
    if mode == Mode::Plain {
        say_resuming(&playing);
        open(&mut conn, &player, &playing, env, paths)?;
        let summary = vec![format!("playing in {} · the download continues", player.name())];
        return Ok(emit(mode, &Doc { summary, ..doc }, &Typed { kind: DocKind::Play, body: &playing }));
    }
    let id = short_ids(&[playing.reply.job.id]).remove(0);
    let _ = ctrlc::set_handler(move || {
        eprintln!("\nThe download continues in the background: torrential status {id}");
        std::process::exit(130);
    });
    let runtime = countdown(&mut conn, &playing)?;
    // While the countdown ran, a TV or a browser that plays the same video kept saving its place.
    if let Some(title) = &mut playing.title
        && let Ok(at) = start_at(&mut conn, &title.video)
    {
        title.resume_at = at;
    }
    say_resuming(&playing);
    open(&mut conn, &player, &playing, env, paths)?;
    let mut out = Doc { next: None, ..play(&playing, known(runtime)) }.human();
    if runtime == Runtime::Unknown {
        out.push_str(&paint(Token::Muted, UNKNOWN_RUNTIME));
        out.push('\n');
    }
    out.push_str(&playing_line(&player.name(), &playing.reply.job));
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

/// Where a title play resumes the video, unless it starts from the beginning.
fn resume_at(playing: &Playing) -> Option<Duration> {
    Duration::try_from_secs_f64(playing.title.as_ref()?.resume_at?).ok()
}

fn say_resuming(playing: &Playing) {
    if let Some(at) = resume_at(playing) {
        anstream::eprintln!("{}", resuming(at));
    }
}

/// Where the person stopped `video` now, unless they finished it.
pub fn start_at(conn: &mut Conn, video: &VideoRef) -> Result<Option<f64>, Error> {
    let page: Box<TitlePage> = conn.ask(&Request::Title { id: video.title.clone() })?;
    Ok(torrential_core::watching::resume_at(&page.watched, video))
}

/// Opens the video and its subtitles in `player`, titled with the release name, showing subtitles in
/// the system language first, and starting where a title play resumes. The engine follows a film or
/// an episode in a player that says where it is, and keeps its place as it keeps a browser's.
pub fn open(conn: &mut Conn, player: &Player, playing: &Playing, env: &Env, paths: &Paths) -> Result<(), Error> {
    let [video, subtitles @ ..] = playing.reply.streams.as_slice() else {
        return Err(Error::internal("the engine streams no file to play"));
    };
    let language = crate::machine::system_language(env);
    let followed = playing.title.as_ref().filter(|_| player.says_where());
    let socket = followed.map(|_| paths.player_socket()).filter(|path| path.as_os_str().len() <= MAX_SOCKET_PATH);
    player.launch(&Launch {
        video: &video.url,
        subtitles: subtitles.iter().map(|s| s.url.as_str()).collect(),
        language: language.as_ref().map(|l| l.code()),
        title: &sanitize(&playing.name),
        start: resume_at(playing),
        socket: socket.as_deref(),
    })?;
    if let (Some(title), Some(socket)) = (followed, socket) {
        // An engine that cannot follow the player leaves the place to the browser and the TV.
        let follow = Request::Follow { video: title.video.clone(), release: Some(title.release.id), socket };
        let _ = conn.ask::<()>(&follow);
    }
    Ok(())
}

/// Picks the video and its subtitles from `listing` and asks the engine to stream them.
pub fn start(conn: &mut Conn, target: Target, listing: &FilesReply, dir: Option<PathBuf>) -> Result<PlayReply, Error> {
    let Some(playable) = stream::pick(&listing.files, Plays::ANY) else {
        let id = short_ref(&listing.id);
        let name = listing.name.clone().unwrap_or_else(|| id.clone());
        return Err(Error::new(ErrorCode::NoSelection, format!("{name} has no video file"))
            .with_hint(format!("run torrential files {id} to list its files"))
            .with_next(format!("torrential files {id}")));
    };
    let files = [playable.video].into_iter().chain(playable.subtitles).collect();
    let reply: Box<PlayReply> = conn.ask(&Request::Play(PlayRequest { target, dir, files }))?;
    Ok(*reply)
}

/// The torrent the words name, with its files and, when a search found it, the result's name. A
/// single word that names a download, a search result, or anything else `get` takes wins over a
/// search for the words, which takes the top video result.
fn resolve(conn: &mut Conn, words: &[String], cwd: &Path) -> Result<(Target, Option<String>, FilesReply), Error> {
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
    let query = SearchQuery { category: Some(Category::Video), ..SearchQuery::new(text.clone()) };
    let result: SearchResult = conn.ask(&Request::Search { query, stream: false })?;
    let releases = result.releases;
    let Some(release) = releases.into_iter().next() else {
        let search = format!("torrential search {}", words.iter().map(|w| shell_word(w)).collect::<Vec<_>>().join(" "));
        return Err(Error::new(ErrorCode::NotFound, format!("no video result matches {text:?}"))
            .with_hint(format!("run {search} to see all results"))
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
fn countdown(conn: &mut Conn, playing: &Playing) -> Result<Runtime, Error> {
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
        let have = file_list(conn, Target::Prefix(id.clone()))?
            .files
            .iter()
            .find(|f| f.index == video.index)
            .map_or(0, |f| f.verified);
        let jobs: Vec<Job> = conn.ask(&Request::Status { refs: vec![id.clone()] })?;
        let rate = jobs.first().map_or(0, |job| job.progress.down_rate);
        let rate = rates.mean(Instant::now(), rate);
        let Some(why) = hold(runtime, video.size, have, rate) else { break };
        let header = Doc { next: None, ..play(playing, known(runtime)) }.human();
        bar.set_message(format!("{header}{}", paint(Token::Accent, &hold_line(why))));
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
    let head = fetch(url, &format!("bytes=0-{}", media::RUNTIME_BYTES - 1)).ok()?;
    media::duration(&head, &[])
        .or_else(|| media::duration(&head, &fetch(url, &format!("bytes=-{}", media::RUNTIME_BYTES)).ok()?))
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

pub const UNKNOWN_RUNTIME: &str = "could not read the video's runtime, so playback may pause while it downloads";

/// The release, the files it plays, and how long the video runs once that is known. A title play
/// also names the source it chose.
pub fn play(playing: &Playing, runtime: Option<Duration>) -> Doc {
    let reply = &playing.reply;
    let id = short_ids(&[reply.job.id]).remove(0);
    let title = vec![Cell::styled(id.clone(), Token::Accent), Cell::new(&playing.name)];
    let mut fields = Vec::new();
    if let Some(title) = &playing.title {
        fields.push(("source", Cell::new(&title.release.name)));
    }
    if let [video, subtitles @ ..] = reply.streams.as_slice() {
        fields.push(("video", Cell::new(format!("{} ({})", video.path, format_size(video.size)))));
        if !subtitles.is_empty() {
            let paths: Vec<&str> = subtitles.iter().map(|s| s.path.as_str()).collect();
            fields.push(("subtitles", Cell::new(paths.join(", "))));
        }
    }
    if let Some(runtime) = runtime {
        fields.push(("runtime", Cell::new(format::clock(runtime))));
    }
    Doc { body: Body::Fields { title, fields }, summary: Vec::new(), next: Some(format!("torrential status {id}")) }
}

pub fn hold_line(hold: Hold) -> String {
    match hold {
        Hold::Reading => "Buffering…".to_string(),
        Hold::Buffering(wait) => format!("Buffering… ready in {}", format::clock(wait)),
        Hold::Countdown(wait) => {
            format!("Starts in {} so playback does not pause. Press Enter to start now", format::clock(wait))
        }
        Hold::Stalled => "Waiting for peers".to_string(),
    }
}

pub fn playing_line(player: &str, job: &Job) -> String {
    let id = short_ids(&[job.id]).remove(0);
    format!(
        "{} Playing in {}. The download continues: torrential status {id}\n",
        paint(Token::Mint, "▶"),
        sanitize(player)
    )
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
