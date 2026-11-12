//! One play of a title: find its sources, try them one at a time best first, move past each that
//! will not start, clean up after it, and play the first that works.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use torrential_core::stream::{self, Playable};
use torrential_core::text::sanitize;
use torrential_core::title::{Episode, Span};
use torrential_core::{
    AttemptEvent, Check, Error, ErrorCode, InfoHash, Job, JobStatus, Lease, Playback, Release, ReleaseId, SearchResult,
    Skip, SourceReport, Swarm, TitleLink, TorrentFile, TorrentId, Tried, VideoRef,
};

use super::super::engine_port::Metainfo;
use super::super::magnet::{self, Magnet};
use super::super::owner::{Command, Created, Finish, MetaTarget, Origin, PlayCommand, Source};
use super::super::search_port::{BoxFuture, Progress};
use super::super::server::{self, Ctx, OwnerHandle};
use super::Resolved;

const MOST_TRIED: usize = 6;
/// Sources after the one under test whose file lists are asked for early, so dead swarms in a row
/// cost one wait.
const LOOK_AHEAD: usize = 2;
/// How long the owner may take to answer with a file list it already has. A source whose file
/// list was asked for early waits at least this long, however long ago that was.
const ANSWER_WITHIN: Duration = Duration::from_millis(100);
/// How often a trial looks for the first bytes of its video.
const LOOK_EVERY: Duration = Duration::from_millis(100);
/// What a browser plays without ffmpeg remuxing it.
const BROWSER_PLAYS: [&str; 4] = ["mp4", "m4v", "mov", "webm"];
/// How long a title's search runs before a strong swarm from any index may go first. The quick
/// indexes have answered by then, and a lawful catalog among them would lead.
const STRONG_AFTER: Duration = Duration::from_millis(2500);

/// What a caller asks a title play for.
pub struct PlayTitle {
    pub video: VideoRef,
    /// Play only this release, with no fallback.
    pub pin: Option<ReleaseId>,
    /// The download that stalled. It is never tried, and once another source plays it is paused.
    pub replace: Option<TorrentId>,
    /// What the caller's player decodes, in place of the preference in config.yaml.
    pub playback: Option<Playback>,
    /// What the caller's player opens.
    pub containers: Containers,
    pub finish: Finish,
}

/// The containers a caller's player opens.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Containers {
    /// A browser's: MP4, M4V, MOV, and WebM, and any other once ffmpeg remuxes it.
    Browser,
    /// Any, as mpv, IINA, and VLC open. A download kept with `get` takes any too.
    Any,
}

impl Containers {
    /// Whether the file at `path` opens, when ffmpeg is there to `remux` it or not.
    fn open(self, path: &str, remux: bool) -> bool {
        let extension = path.rsplit_once('.').map(|(_, extension)| extension.to_ascii_lowercase());
        match self {
            Containers::Any => true,
            Containers::Browser => remux || extension.is_some_and(|e| BROWSER_PLAYS.contains(&e.as_str())),
        }
    }
}

/// Plays `ask` and reports each step, the last being `playing` or `failed`. A download here that
/// holds the video plays first, before any search. Otherwise sources are tried once the search
/// so far holds one that could lead, while slower indexes go on answering, and each next source
/// is the best of what has been found by then. Dropping the future stops the play, and the
/// source under test is put back as it was.
pub async fn run(ctx: &Ctx, ask: PlayTitle, mut report: impl FnMut(AttemptEvent) + Send) {
    let end = Instant::now() + ctx.titles.timing.budget;
    if let Some(id) = ask.replace {
        ctx.titles.failed(ReleaseId::Torrent(id), Skip::Stalled);
    }
    let title = match ctx.titles.resolve(&ask.video, super::super::owner::now()).await {
        Ok(title) => title,
        Err(error) => return report(AttemptEvent::Failed { error, tried: Vec::new() }),
    };
    report(AttemptEvent::Resolved {
        video: title.video.clone(),
        title: title.details.card.clone(),
        episode: title.episode.clone(),
    });
    let jobs = match ctx.owner.ask(|reply| Command::Status { refs: Vec::new(), reply }).await {
        Ok(jobs) => jobs,
        Err(error) => return report(AttemptEvent::Failed { error, tried: Vec::new() }),
    };
    let here: BTreeSet<TorrentId> = jobs.iter().map(|j| j.id).collect();
    let mut walk = Walk { ctx, ask: &ask, title: &title, here: &here, end, tried: Vec::new(), asked: HashMap::new() };
    let healthy: Vec<Job> = jobs.iter().filter(|j| !matches!(j.status, JobStatus::Error { .. })).cloned().collect();
    let held = ctx.titles.line(&healthy, &title.video, Vec::new());
    if let Some(release) = held.into_iter().find(|r| r.failed.is_none() && ask.may_try(r))
        && walk.try_source(release, 1, &mut report).await
    {
        return;
    }

    let preferences = (ctx.web.preferences)();
    let preferences = torrential_core::Preferences { playback: ask.playback.or(preferences.playback), ..preferences };
    let rank_for = || ctx.machine.profile(preferences);
    let (progress, answers) = mpsc::unbounded_channel();
    let mut search = Searching {
        running: Some(ctx.search.search_stream(title.query(preferences), &rank_for, progress)),
        answers,
        started: Instant::now(),
        releases: Vec::new(),
        sources: Vec::new(),
        error: None,
    };
    let line_up = |search: &Searching, tried: &[Tried]| {
        next_up(&ask, ctx.titles.line(&jobs, &title.video, search.releases.clone()), tried)
    };
    loop {
        let ran = search.started.elapsed();
        let first = line_up(&search, &walk.tried).into_iter().next();
        if may_start(first.as_ref(), ran, search.over()) {
            break;
        }
        let strong_after = tokio::time::Instant::from_std(search.started + STRONG_AFTER);
        tokio::select! {
            () = search.next(ctx) => {}
            () = tokio::time::sleep_until(strong_after), if ran < STRONG_AFTER => {}
        }
    }
    if search.releases.is_empty()
        && let Some(error) = search.error.take()
    {
        return report(AttemptEvent::Failed { error, tried: walk.tried });
    }
    report(AttemptEvent::Searched { found: search.releases.len() as u32, sources: search.sources.clone() });
    while Instant::now() < end {
        let candidates = line_up(&search, &walk.tried);
        let Some(release) = candidates.first().cloned() else {
            if search.over() {
                break;
            }
            search.next(ctx).await;
            continue;
        };
        let of = (walk.tried.len() + candidates.len()) as u32;
        walk.look_ahead(&candidates[1..]);
        let trial = walk.try_source(release, of, &mut report);
        tokio::pin!(trial);
        let over = loop {
            tokio::select! {
                over = &mut trial => break over,
                () = search.next(ctx) => {}
            }
        };
        if over {
            return;
        }
    }
    let error = failure(&title.name(), &walk.tried, ask.pin.is_some(), ask.replace.is_some());
    report(AttemptEvent::Failed { error, tried: walk.tried });
}

/// Whether a title play may stop waiting for its search and try `first`, the best of its sources
/// so far, `ran` into the search. A strong swarm in a lawful catalog leads whatever slower
/// indexes find. Any strong swarm may go once the quick indexes have answered. Anything goes once
/// the search is `over`.
fn may_start(first: Option<&Release>, ran: Duration, over: bool) -> bool {
    let lawful = |r: &Release| r.sources.iter().any(|source| torrential_search::lawful(source));
    let strong = first.filter(|r| r.swarm == Some(Swarm::Strong));
    over || strong.is_some_and(|r| lawful(r) || ran >= STRONG_AFTER)
}

/// A title's search as it runs, and the best of what it has found so far.
struct Searching<'a> {
    /// `None` once every source has answered or the deadline has passed.
    running: Option<BoxFuture<'a, Result<SearchResult, Error>>>,
    answers: mpsc::UnboundedReceiver<Progress>,
    started: Instant,
    /// Ranked for playing here.
    releases: Vec<Release>,
    sources: Vec<SourceReport>,
    error: Option<Error>,
}

impl Searching<'_> {
    fn over(&self) -> bool {
        self.running.is_none()
    }

    /// Waits for the next source to answer, and keeps what the search has found by then. Once
    /// the search is over it never returns.
    async fn next(&mut self, ctx: &Ctx) {
        let Some(running) = &mut self.running else { return std::future::pending().await };
        let found = tokio::select! {
            biased;
            Some(Progress { result, .. }) = self.answers.recv() => Ok(result),
            result = running => {
                self.running = None;
                result
            }
        };
        match found {
            Ok(mut result) => {
                server::record(ctx, &mut result);
                self.releases = result.releases;
                self.sources = result.sources;
            }
            Err(error) => self.error = Some(error),
        }
    }
}

/// Where the person stopped `video`, unless they finished it.
fn resume_at(ctx: &Ctx, video: &VideoRef) -> Option<f64> {
    let watched = ctx.titles.shelf.watched(&video.title).ok()?;
    watched.into_iter().find(|w| w.video == *video && !w.finished()).map(|w| w.position)
}

/// A title play's way through its sources.
struct Walk<'a> {
    ctx: &'a Ctx,
    ask: &'a PlayTitle,
    title: &'a Resolved,
    /// The downloads on this machine.
    here: &'a BTreeSet<TorrentId>,
    end: Instant,
    /// The sources that did not play, in the order they were tried.
    tried: Vec<Tried>,
    /// When the play first asked for each source's file list.
    asked: HashMap<ReleaseId, Instant>,
}

impl Walk<'_> {
    /// Asks peers for the file lists of the first of `next` while another source is under test.
    /// The owner keeps what arrives and drops each probe once nobody waits for it, so a probe
    /// never downloads.
    fn look_ahead(&mut self, next: &[Release]) {
        for release in next.iter().take(LOOK_AHEAD) {
            if self.asked.contains_key(&release.id) {
                continue;
            }
            let Some(magnet) = self.magnet_for(release) else { continue };
            let now = Instant::now();
            self.asked.insert(release.id, now);
            let (reply, _) = oneshot::channel();
            let deadline = now + self.ctx.titles.timing.file_list;
            self.ctx.owner.tell(Command::Metadata { target: MetaTarget::Magnet(magnet), deadline, reply });
        }
    }

    /// The magnet a trial of `release` asks peers for its file list with. `None` for a download
    /// here, and for a release with a .torrent file.
    fn magnet_for(&self, release: &Release) -> Option<Magnet> {
        if matches!(release.id, ReleaseId::Torrent(id) if self.here.contains(&id))
            || !matches!(self.ctx.search.torrent_url(release), Ok(None))
        {
            return None;
        }
        match (&release.magnet, release.id) {
            (Some(uri), _) => magnet::parse(uri).ok(),
            (None, ReleaseId::Torrent(id)) => Some(magnet::from_hash(InfoHash::V1(*id.as_bytes()))),
            (None, ReleaseId::Locator(_)) => None,
        }
    }

    /// Tries `release` as the next of `of` sources, and answers whether the play is over: it
    /// plays, or the engine is going away. A source that does not play joins `tried`.
    async fn try_source(&mut self, release: Release, of: u32, report: &mut (impl FnMut(AttemptEvent) + Send)) -> bool {
        let ctx = self.ctx;
        report(AttemptEvent::Trying { rank: self.tried.len() as u32 + 1, of, release: release.clone() });
        let asked = *self.asked.entry(release.id).or_insert_with(Instant::now);
        let mut trial = Trial { owner: &ctx.owner, undo: None };
        match trial.run(self, &release, asked, report).await {
            Ok((job, file)) => {
                trial.undo = None;
                let id = job.id;
                if let Err(error) = ctx.owner.ask(|reply| Command::Played { id, reply }).await {
                    report(AttemptEvent::Failed { error, tried: std::mem::take(&mut self.tried) });
                    return true;
                }
                let video = self.title.video.clone();
                let resume_at = resume_at(ctx, &video);
                report(AttemptEvent::Playing { video, release, job: id, file, resume_at });
                if let Some(old) = self.ask.replace.filter(|old| *old != id) {
                    let _ = ctx.owner.ask(|reply| Command::Pause { refs: vec![old.to_hex()], reply }).await;
                }
                true
            }
            Err(reason) => {
                let reason = *reason;
                trial.undo().await;
                ctx.titles.failed(release.id, reason.clone());
                report(AttemptEvent::Skipped { release: release.id, reason: reason.clone() });
                self.tried.push(Tried { release: release.id, reason });
                false
            }
        }
    }
}

impl PlayTitle {
    /// Whether this play may try `release`: never the download it replaces, and only the pinned
    /// release when one is.
    fn may_try(&self, release: &Release) -> bool {
        self.replace.is_none_or(|old| release.id != ReleaseId::Torrent(old))
            && self.pin.is_none_or(|pin| release.id == pin)
    }
}

/// The sources `ask` tries next, in the order of its `line`: those it may try and has not
/// `tried`, as many as it has tries left.
fn next_up(ask: &PlayTitle, line: Vec<Release>, tried: &[Tried]) -> Vec<Release> {
    let tried_ids: HashSet<ReleaseId> = tried.iter().map(|t| t.release).collect();
    let next = line.into_iter().filter(|r| !tried_ids.contains(&r.id) && ask.may_try(r));
    next.take(MOST_TRIED.saturating_sub(tried.len())).collect()
}

/// The source under test. Dropped before it plays, it puts back what it changed.
struct Trial<'a> {
    owner: &'a OwnerHandle,
    undo: Option<Undo>,
}

/// What a trial changed that it puts back when its source does not play.
enum Undo {
    /// It made the download, which only it may discard.
    Discard(TorrentId),
    /// It resumed a download the person had paused.
    Pause(TorrentId),
}

impl Trial<'_> {
    /// Checks `release` step by step for `walk` and answers the download and the file that play.
    /// Its file list has been on its way since `asked`.
    async fn run(
        &mut self,
        walk: &Walk<'_>,
        release: &Release,
        asked: Instant,
        report: &mut (impl FnMut(AttemptEvent) + Send),
    ) -> Result<(Job, u32), Box<Skip>> {
        let Walk { ctx, ask, title, end, .. } = *walk;
        let timing = ctx.titles.timing;
        report(AttemptEvent::Check { check: Check::FileList });
        let deadline = end.min(asked + timing.file_list).max(Instant::now() + ANSWER_WITHIN);
        let (source, metainfo) = match tokio::time::timeout_at(deadline.into(), files(ctx, release, deadline)).await {
            Ok(Ok(found)) => found,
            Ok(Err(error)) if error.code != ErrorCode::MetadataTimeout => return Err(Skip::Failed { error }.into()),
            _ => return Err(Skip::NoFileList { waited_s: seconds(asked.elapsed()) }.into()),
        };
        let remux = (ctx.web.tools)().ffmpeg.is_some();
        let playable = video_of(&metainfo, title.video.episode, ask.containers, remux)?;

        report(AttemptEvent::Check { check: Check::Start });
        let span = release.span.unwrap_or_else(|| title.video.episode.map_or(Span::Film, Span::Episode));
        let link =
            TitleLink { title: title.video.title.clone(), span, release: Some(release.id), lease: Lease::Borrowed };
        let play = PlayCommand {
            source,
            dir: None,
            files: [playable.video].into_iter().chain(playable.subtitles.iter().copied()).collect(),
            setup_max_size: (ctx.web.preferences)().max_size,
            title: Some(link),
            finish: ask.finish,
        };
        let (job, _, created) = ctx.owner.ask(|reply| Command::Play { play: Box::new(play), reply }).await.map_err(
            |error| match error.code {
                ErrorCode::TooLarge | ErrorCode::DiskFull => Skip::TooLarge { error },
                _ => Skip::Failed { error },
            },
        )?;
        self.undo = match created {
            Created::Yes => Some(Undo::Discard(job.id)),
            Created::Resumed => Some(Undo::Pause(job.id)),
            Created::No => None,
        };

        report(AttemptEvent::Check { check: Check::FirstBytes });
        let begun = Instant::now();
        let within = timing.first_bytes.min(end.saturating_duration_since(begun));
        let mut look = tokio::time::interval(LOOK_EVERY);
        loop {
            look.tick().await;
            let id = job.id;
            let now = match ctx.owner.ask(|reply| Command::Status { refs: vec![id.to_hex()], reply }).await {
                Ok(mut jobs) if !jobs.is_empty() => jobs.remove(0),
                Ok(_) => {
                    let error = Error::new(ErrorCode::NotFound, "the download is gone");
                    return Err(Skip::Failed { error }.into());
                }
                Err(error) => return Err(Skip::Failed { error }.into()),
            };
            let bytes = ctx.engine.file_progress(id).await.ok().and_then(|f| f.get(playable.video as usize).copied());
            if let Some(outcome) = first_bytes(&now.status, bytes.unwrap_or(0), begun.elapsed(), within) {
                return outcome.map(|()| (now, playable.video)).map_err(Box::new);
            }
        }
    }

    /// Puts back what this trial changed, and waits until the owner has. A download the trial
    /// made goes, with its files when the play owns them. When they were on disk before, the
    /// owner removes the download and keeps them.
    async fn undo(&mut self) {
        match self.undo.take() {
            None => {}
            Some(Undo::Discard(id)) => {
                if self.owner.ask(|reply| Command::Discard { id, reply }).await.is_err() {
                    let _ = self.owner.ask(|reply| Command::Remove { refs: vec![id.to_hex()], reply }).await;
                }
            }
            Some(Undo::Pause(id)) => {
                let _ = self.owner.ask(|reply| Command::Pause { refs: vec![id.to_hex()], reply }).await;
            }
        }
    }
}

impl Drop for Trial<'_> {
    fn drop(&mut self) {
        match self.undo.take() {
            None => {}
            Some(Undo::Discard(id)) => {
                let (discarded, _) = oneshot::channel();
                self.owner.tell(Command::Discard { id, reply: discarded });
                // The owner takes commands in order, so this finds the download only if the
                // discard was refused.
                let (removed, _) = oneshot::channel();
                self.owner.tell(Command::Remove { refs: vec![id.to_hex()], reply: removed });
            }
            Some(Undo::Pause(id)) => {
                let (paused, _) = oneshot::channel();
                self.owner.tell(Command::Pause { refs: vec![id.to_hex()], reply: paused });
            }
        }
    }
}

/// The source of `release` and its file list, from a download on this machine, from the
/// release's .torrent file, or from peers.
async fn files(ctx: &Ctx, release: &Release, deadline: Instant) -> Result<(Source, Arc<Metainfo>), Error> {
    if let ReleaseId::Torrent(id) = release.id
        && ctx.owner.ask(|reply| Command::Status { refs: vec![id.to_hex()], reply }).await.is_ok()
    {
        let info = server::metadata(ctx, MetaTarget::Job(id), deadline).await?;
        return Ok((Source::Download(id), info.metainfo));
    }
    match server::release_source(ctx, release.clone(), None).await? {
        Source::Magnet(magnet) => {
            let info = server::metadata(ctx, MetaTarget::Magnet(magnet), deadline).await?;
            let source = match info.download {
                Some(_) => Source::Download(info.id),
                None => Source::Torrent { metainfo: (*info.metainfo).clone(), origin: Origin::Peers },
            };
            Ok((source, info.metainfo))
        }
        Source::Torrent { metainfo, origin } => {
            let shared = Arc::new(metainfo.clone());
            Ok((Source::Torrent { metainfo, origin }, shared))
        }
        Source::Download(id) => {
            let info = server::metadata(ctx, MetaTarget::Job(id), deadline).await?;
            Ok((Source::Download(id), info.metainfo))
        }
    }
}

fn seconds(elapsed: Duration) -> u32 {
    elapsed.as_secs_f64().round() as u32
}

/// The file of the film, or of `episode` in a pack, and its subtitles, or why none plays in a
/// player that opens `containers`. A torrent that holds only sample clips holds no video.
fn video_of(
    metainfo: &Metainfo,
    episode: Option<Episode>,
    containers: Containers,
    remux: bool,
) -> Result<Playable, Box<Skip>> {
    let listing: Vec<TorrentFile> = (0..)
        .zip(&metainfo.files)
        .map(|(index, f)| TorrentFile {
            index,
            path: sanitize(&f.path),
            size: f.size,
            selected: false,
            verified: 0,
            risk: None,
        })
        .collect();
    let playable = match episode {
        Some(episode) => stream::episode(&listing, &sanitize(&metainfo.name), episode),
        None => stream::pick(&listing),
    };
    let playable = playable.filter(|p| !stream::is_sample(&listing[p.video as usize].path)).ok_or(Skip::NoVideo)?;
    let path = &listing[playable.video as usize].path;
    if containers.open(path, remux) {
        Ok(playable)
    } else {
        let container = path.rsplit_once('.').map_or("", |(_, extension)| extension).to_ascii_lowercase();
        Err(Skip::Unplayable { container }.into())
    }
}

/// Whether a download under test has started: the video has bytes, or the download is done. A
/// download that fails, or that gets nothing `within` its start, will not do.
fn first_bytes(status: &JobStatus, video_bytes: u64, waited: Duration, within: Duration) -> Option<Result<(), Skip>> {
    match status {
        JobStatus::Done { .. } => Some(Ok(())),
        _ if video_bytes > 0 => Some(Ok(())),
        JobStatus::Error { error } => Some(Err(Skip::Failed { error: error.clone() })),
        _ if waited >= within => Some(Err(Skip::NoData { waited_s: seconds(waited) })),
        _ => None,
    }
}

/// Why no source played `name`.
fn failure(name: &str, tried: &[Tried], pinned: bool, replacing: bool) -> Error {
    let every = |skip: fn(&Skip) -> bool| !tried.is_empty() && tried.iter().all(|t| skip(&t.reason));
    if tried.is_empty() {
        return Error::new(ErrorCode::NoSource, format!("no torrent of {name} was found"))
            .with_hint("search the torrents for it by name");
    }
    if every(|s| matches!(s, Skip::Unplayable { .. })) {
        return Error::new(ErrorCode::NoSource, format!("no source of {name} plays in the browser without ffmpeg"))
            .with_hint("install ffmpeg, then play it again");
    }
    if every(|s| matches!(s, Skip::TooLarge { .. }))
        && let Some(Skip::TooLarge { error }) = tried.first().map(|t| &t.reason)
    {
        return error.clone();
    }
    let (message, hint) = match (pinned, replacing) {
        (true, _) => (format!("this source of {name} does not play now"), "let torrential choose a source"),
        (false, true) => {
            (format!("no other source of {name} plays now"), "keep waiting on this one, or try again later")
        }
        (false, false) => (format!("no source of {name} plays now"), "try again later, or choose a source"),
    };
    Error::new(ErrorCode::NoSource, message).with_hint(hint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::engine_port::FakeTorrentFile;
    use torrential_core::InfoHash;

    fn release(n: u8) -> Release {
        Release {
            id: ReleaseId::Torrent(TorrentId::from_bytes([n; 20])),
            name: format!("release {n}"),
            size: None,
            seeders: None,
            leechers: None,
            published: None,
            category: None,
            flags: BTreeSet::new(),
            sources: Vec::new(),
            magnet: None,
            torrent_url: None,
            info_url: None,
            span: None,
            swarm: None,
            failed: None,
        }
    }

    fn numbers(releases: Vec<Release>) -> Vec<u8> {
        releases.iter().map(|r| r.name.trim_start_matches("release ").parse().unwrap()).collect()
    }

    #[test]
    fn a_play_tries_at_most_six_sources_never_the_one_it_replaces_and_with_a_pin_only_that() {
        let ask = |replace: Option<u8>, pin: Option<u8>| PlayTitle {
            video: "tt0063350".parse().unwrap(),
            pin: pin.map(|n| release(n).id),
            replace: replace.map(|n| TorrentId::from_bytes([n; 20])),
            playback: None,
            containers: Containers::Browser,
            finish: Finish::Stream,
        };
        let line: Vec<Release> = (1..=12).map(release).collect();
        type Row<'a> = (Option<u8>, Option<u8>, &'a [u8], &'a [u8]);
        let table: [Row; 5] = [
            (None, None, &[], &[1, 2, 3, 4, 5, 6]),
            (None, None, &[1, 4], &[2, 3, 5, 6]),
            (Some(2), None, &[], &[1, 3, 4, 5, 6, 7]),
            (None, Some(9), &[], &[9]),
            (None, Some(9), &[9], &[]),
        ];
        for (replace, pin, tried, expected) in table {
            let tried: Vec<Tried> =
                tried.iter().map(|n| Tried { release: release(*n).id, reason: Skip::NoVideo }).collect();
            let next = numbers(next_up(&ask(replace, pin), line.clone(), &tried));
            assert_eq!(next, expected, "replacing {replace:?}, pinned to {pin:?}, {} tried", tried.len());
        }
    }

    #[test]
    fn a_play_starts_before_its_search_ends_only_on_a_source_that_could_lead() {
        let listed = |source: &str, seeders: Option<u32>| Release {
            seeders,
            sources: vec![source.into()],
            swarm: Swarm::of(seeders, torrential_search::lawful(source)),
            ..release(1)
        };
        let archive = listed("archive.org", None);
        let crowded = listed("knaben", Some(40));
        let fair = listed("knaben", Some(9));
        let counted_none = listed("archive.org", Some(0));
        let here = release(2);
        let ms = Duration::from_millis;
        let table = [
            (Some(&archive), ms(0), false, true),
            (Some(&crowded), ms(0), false, false),
            (Some(&crowded), ms(2499), false, false),
            (Some(&crowded), ms(2500), false, true),
            (Some(&fair), ms(7000), false, false),
            (Some(&fair), ms(10), true, true),
            (Some(&counted_none), ms(7000), false, false),
            (Some(&here), ms(7000), false, false),
            (None, ms(7000), false, false),
            (None, ms(10), true, true),
        ];
        for (n, (first, ran, over, expected)) in table.into_iter().enumerate() {
            assert_eq!(may_start(first, ran, over), expected, "row {n}");
        }
    }

    fn torrent(name: &str, files: &[(&str, u64)]) -> Metainfo {
        let files = files.iter().map(|(path, size)| (path.to_string(), *size)).collect();
        FakeTorrentFile { info_hash: InfoHash::V1([7; 20]), name: name.into(), files }.metainfo().unwrap()
    }

    #[test]
    fn the_video_is_the_film_or_the_episode_never_a_sample_and_a_browser_without_ffmpeg_opens_only_its_containers() {
        let film = torrent("Night", &[("Night.mp4", 900), ("Night.en.srt", 9), ("Sample.mp4", 99)]);
        let pack = torrent(
            "Harbor.Lights.S01.1080p",
            &[("Harbor.Lights.S01E01.mkv", 900), ("Harbor.Lights.S01E02.mp4", 900), ("Harbor.Lights.S01E02.srt", 9)],
        );
        let disc = torrent("Night DVD", &[("NIGHT.iso", 900), ("readme.txt", 9)]);
        let samples = torrent("Night", &[("Sample/Night.sample.mp4", 90), ("sample-night.mkv", 80)]);
        let episode = |season, episode| Some(Episode { season, episode });
        let read = |m: &Metainfo, e, remux| video_of(m, e, Containers::Browser, remux).map(|p| (p.video, p.subtitles));
        let table = [
            (read(&film, None, false), Ok((0, vec![1]))),
            (read(&pack, episode(1, 2), false), Ok((1, vec![2]))),
            (read(&pack, episode(1, 1), true), Ok((0, vec![2]))),
            (read(&pack, episode(1, 1), false), Err(Box::new(Skip::Unplayable { container: "mkv".into() }))),
            (read(&pack, episode(1, 3), true), Err(Box::new(Skip::NoVideo))),
            (read(&disc, None, true), Err(Box::new(Skip::NoVideo))),
            (read(&samples, None, true), Err(Box::new(Skip::NoVideo))),
        ];
        for (n, (got, expected)) in table.into_iter().enumerate() {
            assert_eq!(got, expected, "row {n}");
        }
        let paths = ["a.mp4", "a.M4V", "a.mov", "a.webm", "a.mkv", "a.avi", "mp4"];
        let open = |containers: Containers, remux| paths.map(|p| containers.open(p, remux));
        assert_eq!(open(Containers::Browser, false), [true, true, true, true, false, false, false]);
        assert_eq!(open(Containers::Browser, true), [true; 7]);
        assert_eq!(open(Containers::Any, false), [true; 7]);
    }

    #[test]
    fn a_download_under_test_starts_with_its_first_bytes_and_not_later_than_its_deadline() {
        let within = Duration::from_secs(30);
        let error = Error::new(ErrorCode::StorageError, "cannot write");
        let downloading = JobStatus::Downloading;
        let table = [
            (JobStatus::Done { finished_at: 1 }, 0, 0, Some(Ok(()))),
            (downloading.clone(), 1, 0, Some(Ok(()))),
            (downloading.clone(), 0, 29, None),
            (JobStatus::Metadata, 0, 29, None),
            (downloading.clone(), 0, 30, Some(Err(Skip::NoData { waited_s: 30 }))),
            (JobStatus::Error { error: error.clone() }, 0, 1, Some(Err(Skip::Failed { error }))),
        ];
        for (status, bytes, waited, expected) in table {
            assert_eq!(first_bytes(&status, bytes, Duration::from_secs(waited), within), expected, "{status:?}");
        }
    }

    #[test]
    fn a_failed_play_says_why_no_source_played() {
        let tried = |reason: Skip| Tried { release: release(1).id, reason };
        let too_large = Error::new(ErrorCode::DiskFull, "not enough space");
        let read = |tried: &[Tried], pinned, replacing| {
            let e = failure("Night of the Living Dead (1968)", tried, pinned, replacing);
            (e.code, e.message)
        };
        let no_source = |message: &str| (ErrorCode::NoSource, message.to_string());
        assert_eq!(read(&[], false, false), no_source("no torrent of Night of the Living Dead (1968) was found"));
        assert_eq!(
            read(&[tried(Skip::Unplayable { container: "mkv".into() })], false, false),
            no_source("no source of Night of the Living Dead (1968) plays in the browser without ffmpeg")
        );
        assert_eq!(
            read(&[tried(Skip::TooLarge { error: too_large.clone() })], false, false),
            (ErrorCode::DiskFull, "not enough space".into())
        );
        let mixed = [tried(Skip::NoVideo), tried(Skip::NoData { waited_s: 30 })];
        assert_eq!(read(&mixed, false, false), no_source("no source of Night of the Living Dead (1968) plays now"));
        assert_eq!(
            read(&mixed[..1], true, false),
            no_source("this source of Night of the Living Dead (1968) does not play now")
        );
        assert_eq!(
            read(&mixed, false, true),
            no_source("no other source of Night of the Living Dead (1968) plays now")
        );
    }
}
