//! One play of a title: find its sources, try them one at a time best first, move past each that
//! will not start, clean up after it, and play the first that works.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use torrential_core::stream::{self, Playable};
use torrential_core::text::sanitize;
use torrential_core::title::{Episode, Span};
use torrential_core::{
    AttemptEvent, Category, Check, Error, ErrorCode, Job, JobStatus, Lease, Playback, Release, ReleaseId, Skip,
    TitleLink, TorrentFile, TorrentId, Tried, VideoRef,
};

use super::super::engine_port::Metainfo;
use super::super::owner::{Command, Created, MetaTarget, Origin, PlayCommand, Source};
use super::super::server::{self, Ctx, OwnerHandle};
use super::Resolved;

/// Ranked releases looked at. The rest rarely play better than these.
const MOST_CONSIDERED: usize = 12;
const MOST_TRIED: usize = 6;
/// How often a trial looks for the first bytes of its video.
const LOOK_EVERY: Duration = Duration::from_millis(100);
/// What the browser plays without ffmpeg remuxing it.
const BROWSER_PLAYS: [&str; 4] = ["mp4", "m4v", "mov", "webm"];

/// What a caller asks a title play for.
pub struct PlayTitle {
    pub video: VideoRef,
    /// Play only this release, with no fallback.
    pub pin: Option<ReleaseId>,
    /// The download that stalled. It is never tried, and once another source plays it is paused.
    pub replace: Option<TorrentId>,
    /// What the caller's player decodes, in place of the preference in config.yaml.
    pub playback: Option<Playback>,
}

/// Plays `ask` and reports each step, the last being `playing` or `failed`. Dropping the future
/// stops the play, and the download under test is discarded if the play made it.
pub async fn run(ctx: &Ctx, ask: PlayTitle, mut report: impl FnMut(AttemptEvent) + Send) {
    let started = Instant::now();
    let timing = ctx.titles.timing;
    let end = started + timing.budget;
    if let Some(id) = ask.replace {
        ctx.titles.failed(ReleaseId::Torrent(id));
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
    let preferences = (ctx.web.preferences)();
    let preferences = torrential_core::Preferences { playback: ask.playback.or(preferences.playback), ..preferences };
    let rank_for = || ctx.machine.profile(preferences);
    let mut result = match ctx.search.search(title.query(preferences), &rank_for).await {
        Ok(result) => result,
        Err(error) => return report(AttemptEvent::Failed { error, tried: Vec::new() }),
    };
    server::record(ctx, &mut result);
    report(AttemptEvent::Searched { found: result.releases.len() as u32, sources: result.sources });

    let jobs = match ctx.owner.ask(|reply| Command::Status { refs: Vec::new(), reply }).await {
        Ok(jobs) => jobs,
        Err(error) => return report(AttemptEvent::Failed { error, tried: Vec::new() }),
    };
    let here: BTreeSet<TorrentId> = jobs.iter().map(|j| j.id).collect();
    let linked = jobs.iter().filter_map(|job| holding(job, &title.video)).collect();
    let line = Line {
        here: &here,
        failed_lately: &|id| ctx.titles.failed_lately(id),
        replace: ask.replace,
        pin: ask.pin.as_ref(),
    };
    let candidates = line.order(linked, result.releases);
    let of = candidates.len() as u32;
    let mut tried = Vec::new();
    for (rank, release) in (1..).zip(candidates) {
        if Instant::now() >= end {
            break;
        }
        report(AttemptEvent::Trying { rank, of, release: release.clone() });
        let mut trial = Trial { owner: &ctx.owner, made: None };
        match trial.run(ctx, &title, &release, end, &mut report).await {
            Ok((job, file)) => {
                trial.made = None;
                let id = job.id;
                if let Err(error) = ctx.owner.ask(|reply| Command::Played { id, reply }).await {
                    return report(AttemptEvent::Failed { error, tried });
                }
                let resume_at = resume_at(ctx, &title.video);
                report(AttemptEvent::Playing { video: title.video.clone(), release, job: id, file, resume_at });
                if let Some(old) = ask.replace.filter(|old| *old != id) {
                    let _ = ctx.owner.ask(|reply| Command::Pause { refs: vec![old.to_hex()], reply }).await;
                }
                return;
            }
            Err(reason) => {
                let reason = *reason;
                trial.discard().await;
                ctx.titles.failed(release.id);
                report(AttemptEvent::Skipped { release: release.id, reason: reason.clone() });
                tried.push(Tried { release: release.id, reason });
            }
        }
    }
    let error = failure(&title.name(), &tried, ask.pin.is_some(), ask.replace.is_some());
    report(AttemptEvent::Failed { error, tried });
}

/// Where the person stopped `video`, unless they finished it.
fn resume_at(ctx: &Ctx, video: &VideoRef) -> Option<f64> {
    let watched = ctx.titles.shelf.watched(&video.title).ok()?;
    watched.into_iter().find(|w| w.video == *video && !w.finished()).map(|w| w.position)
}

/// A download on this machine that a title play made for `video`'s title and that holds it, as a
/// release to try.
fn holding(job: &Job, video: &VideoRef) -> Option<Release> {
    let link = job.title.as_ref().filter(|link| link.title == video.title && link.span.covers(video.episode))?;
    Some(Release {
        id: ReleaseId::Torrent(job.id),
        name: job.name.clone().unwrap_or_else(|| job.id.to_hex()),
        size: job.progress.size,
        seeders: None,
        leechers: None,
        published: None,
        category: Some(Category::Video),
        flags: BTreeSet::new(),
        sources: Vec::new(),
        magnet: None,
        torrent_url: None,
        info_url: None,
        span: Some(link.span),
    })
}

/// What decides the order sources are tried in.
struct Line<'a> {
    /// The downloads on this machine.
    here: &'a BTreeSet<TorrentId>,
    failed_lately: &'a (dyn Fn(&ReleaseId) -> bool + Sync),
    replace: Option<TorrentId>,
    pin: Option<&'a ReleaseId>,
}

impl Line<'_> {
    /// The releases to try, best first: downloads on this machine that hold the video, then the
    /// ranked releases, and last those that failed lately. Of the ranked ones only the top are
    /// looked at, and only the first few are tried. The stalled download is never tried, and a
    /// pinned release is the only one.
    fn order(&self, linked: Vec<Release>, ranked: Vec<Release>) -> Vec<Release> {
        let mut seen = HashSet::new();
        let considered = ranked.into_iter().take(MOST_CONSIDERED);
        let mut line: Vec<Release> = linked
            .into_iter()
            .chain(considered)
            .filter(|r| seen.insert(r.id))
            .filter(|r| self.replace.is_none_or(|old| r.id != ReleaseId::Torrent(old)))
            .filter(|r| self.pin.is_none_or(|pin| r.id == *pin))
            .collect();
        let here = |r: &Release| matches!(r.id, ReleaseId::Torrent(id) if self.here.contains(&id));
        line.sort_by_key(|r| ((self.failed_lately)(&r.id), !here(r)));
        line.truncate(MOST_TRIED);
        line
    }
}

/// The source under test. Dropped before it plays, it discards the download it made.
struct Trial<'a> {
    owner: &'a OwnerHandle,
    /// The download this trial made, which only it may discard.
    made: Option<TorrentId>,
}

impl Trial<'_> {
    /// Checks `release` step by step and answers the download and the file that play.
    async fn run(
        &mut self,
        ctx: &Ctx,
        title: &Resolved,
        release: &Release,
        end: Instant,
        report: &mut (impl FnMut(AttemptEvent) + Send),
    ) -> Result<(Job, u32), Box<Skip>> {
        let timing = ctx.titles.timing;
        report(AttemptEvent::Check { check: Check::FileList });
        let asked = Instant::now();
        let deadline = end.min(asked + timing.file_list);
        let (source, metainfo) = match tokio::time::timeout_at(deadline.into(), files(ctx, release, deadline)).await {
            Ok(Ok(found)) => found,
            Ok(Err(error)) if error.code != ErrorCode::MetadataTimeout => return Err(Skip::Failed { error }.into()),
            _ => return Err(Skip::NoFileList { waited_s: seconds(asked.elapsed()) }.into()),
        };
        let remux = (ctx.web.tools)().ffmpeg.is_some();
        let playable = video_of(&metainfo, title.video.episode, remux)?;

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
        };
        let (job, _, created) = ctx.owner.ask(|reply| Command::Play { play: Box::new(play), reply }).await.map_err(
            |error| match error.code {
                ErrorCode::TooLarge | ErrorCode::DiskFull => Skip::TooLarge { error },
                _ => Skip::Failed { error },
            },
        )?;
        if created == Created::Yes {
            self.made = Some(job.id);
        }

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

    /// Lets go of the download this trial made, and waits until the owner has. The owner deletes
    /// its files when the play owns them. When they were on disk before, it removes the download
    /// and keeps them.
    async fn discard(&mut self) {
        let Some(id) = self.made.take() else { return };
        if self.owner.ask(|reply| Command::Discard { id, reply }).await.is_err() {
            let _ = self.owner.ask(|reply| Command::Remove { refs: vec![id.to_hex()], reply }).await;
        }
    }
}

impl Drop for Trial<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.made.take() {
            let (discarded, _) = oneshot::channel();
            self.owner.tell(Command::Discard { id, reply: discarded });
            // The owner takes commands in order, so this finds the download only if the discard
            // was refused.
            let (removed, _) = oneshot::channel();
            self.owner.tell(Command::Remove { refs: vec![id.to_hex()], reply: removed });
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

/// The file of the film, or of `episode` in a pack, and its subtitles, or why none plays. A
/// torrent that holds only sample clips holds no video.
fn video_of(metainfo: &Metainfo, episode: Option<Episode>, remux: bool) -> Result<Playable, Box<Skip>> {
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
    if container_plays(path, remux) {
        Ok(playable)
    } else {
        let container = path.rsplit_once('.').map_or("", |(_, extension)| extension).to_ascii_lowercase();
        Err(Skip::Unplayable { container }.into())
    }
}

/// Whether the browser plays the file at `path`, directly or once ffmpeg remuxes it.
fn container_plays(path: &str, remux: bool) -> bool {
    let extension = path.rsplit_once('.').map(|(_, extension)| extension.to_ascii_lowercase());
    remux || extension.is_some_and(|e| BROWSER_PLAYS.contains(&e.as_str()))
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
        }
    }

    fn numbers(releases: Vec<Release>) -> Vec<u8> {
        releases.iter().map(|r| r.name.trim_start_matches("release ").parse().unwrap()).collect()
    }

    #[test]
    fn sources_go_downloads_here_first_failures_last_and_at_most_six() {
        let here: BTreeSet<TorrentId> = [3, 40].map(|n| TorrentId::from_bytes([n; 20])).into();
        let failed: HashSet<ReleaseId> = [2, 40].map(|n| release(n).id).into();
        let failed_lately = |id: &ReleaseId| failed.contains(id);
        let order = |replace: Option<u8>, pin: Option<u8>, linked: &[u8], ranked: &[u8]| {
            let pin = pin.map(|n| release(n).id);
            let line = Line {
                here: &here,
                failed_lately: &failed_lately,
                replace: replace.map(|n| TorrentId::from_bytes([n; 20])),
                pin: pin.as_ref(),
            };
            numbers(
                line.order(linked.iter().map(|n| release(*n)).collect(), ranked.iter().map(|n| release(*n)).collect()),
            )
        };
        let many: Vec<u8> = (1..=20).collect();
        let late: Vec<u8> = [vec![2], (20..=40).collect()].concat();
        type Row<'a> = (Option<u8>, Option<u8>, &'a [u8], &'a [u8], &'a [u8]);
        let table: [Row; 7] = [
            (None, None, &[], &[1, 2, 3, 4], &[3, 1, 4, 2]),
            (None, None, &[40], &[1, 2, 40], &[1, 40, 2]),
            (None, None, &[], &many, &[3, 1, 4, 5, 6, 7]),
            (None, None, &[], &late, &[20, 21, 22, 23, 24, 25]),
            (Some(3), None, &[], &[1, 2, 3, 4], &[1, 4, 2]),
            (None, Some(2), &[], &[1, 2, 3], &[2]),
            (None, Some(30), &[], &many, &[]),
        ];
        for (replace, pin, linked, ranked, expected) in table {
            assert_eq!(order(replace, pin, linked, ranked), expected, "linked {linked:?}, ranked {ranked:?}");
        }
    }

    fn torrent(name: &str, files: &[(&str, u64)]) -> Metainfo {
        let files = files.iter().map(|(path, size)| (path.to_string(), *size)).collect();
        FakeTorrentFile { info_hash: InfoHash::V1([7; 20]), name: name.into(), files }.metainfo().unwrap()
    }

    #[test]
    fn the_video_is_the_film_or_the_episode_never_a_sample_and_plays_without_ffmpeg_only_in_a_browser_container() {
        let film = torrent("Night", &[("Night.mp4", 900), ("Night.en.srt", 9), ("Sample.mp4", 99)]);
        let pack = torrent(
            "Harbor.Lights.S01.1080p",
            &[("Harbor.Lights.S01E01.mkv", 900), ("Harbor.Lights.S01E02.mp4", 900), ("Harbor.Lights.S01E02.srt", 9)],
        );
        let disc = torrent("Night DVD", &[("NIGHT.iso", 900), ("readme.txt", 9)]);
        let samples = torrent("Night", &[("Sample/Night.sample.mp4", 90), ("sample-night.mkv", 80)]);
        let episode = |season, episode| Some(Episode { season, episode });
        let read = |m: &Metainfo, e, remux| video_of(m, e, remux).map(|p| (p.video, p.subtitles));
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
        let plays = ["a.mp4", "a.M4V", "a.mov", "a.webm", "a.mkv", "a.avi", "mp4"].map(|p| container_plays(p, false));
        assert_eq!(plays, [true, true, true, true, false, false, false]);
        assert!(container_plays("a.mkv", true));
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
