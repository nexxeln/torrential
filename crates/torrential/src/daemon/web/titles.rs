//! Films and series for the web app: finding one, its page, its pictures, its sources, playing
//! it, where the person stopped, and what they saved.

use std::io;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use torrential_core::title::Episode;
use torrential_core::watching::next;
use torrential_core::{
    ArtKind, AttemptEvent, EpisodeInfo, Error, ErrorCode, Kind, Next, Playback, Player, ReleaseId, TitleCard,
    TitleDetails, TitleId, TorrentId, VideoRef, Viewer, Watched,
};

use super::super::devices::Asker;
use super::super::http::{Connection, Request};
use super::super::owner::{Command, Finish, now};
use super::super::server::{self, Ctx};
use super::super::titles::attempt::{self, Containers, PlayTitle};
use super::super::titles::catalog_port::label;
use super::{NDJSON, fail, line, number, required};
use crate::output::views::title_name;

/// How long a browser keeps a picture. A title's pictures rarely change.
const ART_CACHE: &str = "Cache-Control: private, max-age=2592000\r\n";
/// Titles listed under Continue on the home page.
const CONTINUE_MOST: usize = 20;

/// `GET /api/titles?q=`: films and series named like the words. With title lookups off in
/// config.yaml it finds none, and says it is offline.
pub async fn search(ctx: &Ctx, request: &Request) -> Result<Value, Error> {
    let words = required(request, "q")?;
    if !ctx.titles.catalog.online() {
        return Ok(json!({ "titles": [], "offline": true }));
    }
    Ok(json!({ "titles": ctx.titles.catalog.search(words.trim()).await? }))
}

/// `GET /api/title?id=`.
pub async fn page(ctx: &Ctx, request: &Request) -> Result<Value, Error> {
    let id: TitleId = required(request, "id")?.parse()?;
    super::value(server::title_page(ctx, id).await?)
}

/// `GET /api/title/art?id=&kind=[&season=&episode=]`: a picture, which the browser keeps.
pub async fn art(conn: &mut Connection<'_>, request: &Request) -> io::Result<bool> {
    let ctx = conn.ctx;
    let (id, kind) = match art_of(request) {
        Ok(asked) => asked,
        Err(error) => return fail(conn, request, &error).await,
    };
    let picture = tokio::select! {
        picture = ctx.titles.catalog.art(&id, kind) => picture,
        gone = conn.gone() => return Err(gone),
    };
    match picture {
        Ok(picture) => conn.send("200 OK", picture.format.mime(), ART_CACHE, &picture.bytes, request).await,
        // A missing picture, a failed fetch, and lookups turned off all leave the page to draw a plain card.
        Err(error) => conn.json("404 Not Found", &json!({ "error": error }), request).await,
    }
}

fn art_of(request: &Request) -> Result<(TitleId, ArtKind), Error> {
    let id = required(request, "id")?.parse()?;
    let kind = match required(request, "kind")?.as_str() {
        "poster" => ArtKind::Poster,
        "background" => ArtKind::Background,
        "logo" => ArtKind::Logo,
        "thumbnail" => {
            let numbered = |name| {
                number(request, name)?
                    .ok_or_else(|| Error::new(ErrorCode::Usage, format!("a thumbnail needs the {name} parameter")))
            };
            ArtKind::Thumbnail(Episode { season: numbered("season")?, episode: numbered("episode")? })
        }
        other => {
            return Err(Error::new(
                ErrorCode::Usage,
                format!("{other} is not a kind of picture: use poster, background, logo, or thumbnail"),
            ));
        }
    };
    Ok((id, kind))
}

/// `GET /api/title/sources?video=[&width=&height=&scale=&playback=]`: the title's releases as
/// `/api/search` streams them, in the order a play from that browser tries them.
pub async fn sources(conn: &mut Connection<'_>, request: &Request, asker: &Asker) -> io::Result<bool> {
    let ctx = conn.ctx;
    let asked = async {
        let video: VideoRef = required(request, "video")?.parse()?;
        let player = listed_for(request, asker)?;
        let title = ctx.titles.resolve(&video, now()).await?;
        let jobs = ctx.owner.ask(|reply| Command::Status { refs: Vec::new(), reply }).await?;
        let query = ctx.titles.query(&title, (ctx.web.preferences)(), player).await?;
        Ok::<_, Error>((query, title.video, jobs))
    };
    let asked = tokio::select! {
        asked = asked => asked,
        gone = conn.gone() => return Err(gone),
    };
    match asked {
        Ok((query, video, jobs)) => {
            let (player, ffmpeg) = (query.profile.player, (ctx.web.tools)().ffmpeg.is_some());
            let line = |releases| ctx.titles.line(&jobs, &video, releases, player, ffmpeg);
            super::results(conn, request, query, line).await
        }
        Err(error) => fail(conn, request, &error).await,
    }
}

/// The screen that pressed play and what its player decodes, as the web app measures them.
#[derive(Deserialize)]
struct Measured {
    /// The screen in device pixels.
    width: u32,
    height: u32,
    /// Device pixels to a CSS pixel.
    scale: f64,
    playback: Playback,
}

impl Measured {
    fn viewer(self) -> Result<Viewer, Error> {
        let Measured { width, height, scale, playback } = self;
        let usage = |message: String| Err(Error::new(ErrorCode::Usage, message));
        if width == 0 || height == 0 {
            return usage(format!("a {width}x{height} screen has no pixels"));
        }
        if !(scale.is_finite() && scale > 0.0) {
            return usage(format!("scale {scale} is not a number of device pixels to a CSS pixel"));
        }
        Ok(Viewer::measured(width, height, scale, playback))
    }
}

/// Who a source listing ranks for, from its `width`, `height`, `scale`, and `playback`
/// parameters. The screen's three come with `playback` or not at all.
fn listed_for(request: &Request, asker: &Asker) -> Result<Player, Error> {
    let size = (number(request, "width")?, number(request, "height")?, number(request, "scale")?);
    let playback = request.param("playback").map(|text| playback(&text)).transpose()?;
    let viewer = match (size, playback) {
        ((Some(width), Some(height), Some(scale)), Some(playback)) => {
            Some(Measured { width, height, scale, playback }.viewer()?)
        }
        ((None, None, None), _) => None,
        _ => return Err(Error::new(ErrorCode::Usage, "a viewer needs all of width, height, scale, and playback")),
    };
    Ok(player(viewer, playback, asker))
}

/// Who a web play or source listing ranks for. Every browser decodes what it reports. A browser
/// on this Mac shows this Mac's display, so only a paired device's screen counts. A tab opened
/// before browsers reported their screen reports only what it decodes. A request that reports
/// neither ranks as `torrential play` does.
fn player(viewer: Option<Viewer>, playback: Option<Playback>, asker: &Asker) -> Player {
    match (viewer, playback) {
        (Some(viewer), _) if *asker != Asker::Local => Player::Device(viewer),
        (Some(Viewer { playback, .. }), _) | (None, Some(playback)) => Player::Browser { playback },
        (None, None) => Player::External,
    }
}

fn playback(text: &str) -> Result<Playback, Error> {
    serde_json::from_value(Value::String(text.to_string()))
        .map_err(|_| Error::new(ErrorCode::Usage, format!("{text} is not a playback: use h264, hevc, or hevc-hdr")))
}

#[derive(Deserialize)]
struct Play {
    /// `tt0063350`, `tt0903747:3:5`, or a series for its next episode.
    video: VideoRef,
    /// The screen that pressed play and what its browser decodes.
    viewer: Option<Measured>,
    /// What the browser decodes, from a tab opened before browsers reported their screen.
    playback: Option<Playback>,
    /// Play only this release.
    source: Option<ReleaseId>,
    /// The download that stalled.
    replace: Option<TorrentId>,
}

/// `POST /api/title/play`: a line of JSON for each step of the play, the last being `playing` or
/// `failed`. Closing the connection stops the play and gives up the source under test.
pub async fn play(conn: &mut Connection<'_>, request: &Request, asker: &Asker) -> io::Result<bool> {
    let ctx = conn.ctx;
    let ask = super::body::<Play>(request).and_then(|Play { video, viewer, playback, source, replace }| {
        let player = player(viewer.map(Measured::viewer).transpose()?, playback, asker);
        Ok(PlayTitle { video, pin: source, replace, player, containers: Containers::Browser, finish: Finish::Stream })
    });
    let ask = match ask {
        Ok(ask) => ask,
        Err(error) => return fail(conn, request, &error).await,
    };
    conn.begin(NDJSON, request).await?;
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let attempt = attempt::run(ctx, ask, move |event| {
        let _ = events_tx.send(event);
    });
    tokio::pin!(attempt);
    loop {
        tokio::select! {
            biased;
            Some(event) = events.recv() => line(conn, &step(ctx, event).await).await?,
            () = &mut attempt => break,
            gone = conn.gone() => return Err(gone),
        }
    }
    while let Ok(event) = events.try_recv() {
        line(conn, &step(ctx, event).await).await?;
    }
    conn.end().await
}

/// A step as the web app reads it. The player gets where the video is served in place of the
/// download and file that play.
async fn step(ctx: &Ctx, event: AttemptEvent) -> Value {
    let AttemptEvent::Playing { video, release, job, file, resume_at } = event else {
        return serde_json::to_value(event).expect("steps serialize");
    };
    match super::play::playing_file(ctx, job, file).await {
        Ok(play) => {
            json!({ "type": "playing", "video": video, "release": release, "play": play, "resume_at": resume_at })
        }
        Err(error) => json!({ "type": "failed", "error": error, "tried": [] }),
    }
}

#[derive(Deserialize)]
pub struct Position {
    video: VideoRef,
    /// Seconds.
    position: f64,
    duration: Option<f64>,
    release: Option<ReleaseId>,
}

/// `POST /api/watched`: where a film or episode stopped. The player sends it every few seconds.
pub async fn watched(ctx: &Ctx, request: Position) -> Result<Value, Error> {
    let Position { video, position, duration, release } = request;
    check_times(position, duration)?;
    let now = now();
    watchable(&ctx.titles.catalog.title(&video.title).await?, video.episode, now)?;
    let watched = Watched { video, position, duration, release, at: now };
    ctx.titles.shelf.watch(&watched)?;
    Ok(json!({ "watched": watched }))
}

/// No video runs longer, in seconds.
const LONGEST: f64 = 1_000.0 * 3_600.0;

/// Fails with `usage` unless a video can stop at `position` and run for `duration`: both are
/// finite, neither is negative or over 1,000 hours, and the position is not past the duration.
fn check_times(position: f64, duration: Option<f64>) -> Result<(), Error> {
    let usage = |message: String| Err(Error::new(ErrorCode::Usage, message));
    if !(0.0..=LONGEST).contains(&position) {
        return usage(format!("position {position} is not a time in a video"));
    }
    match duration {
        Some(length) if !(length > 0.0 && length <= LONGEST) => {
            usage(format!("duration {length} is not the length of a video"))
        }
        Some(length) if position > length => {
            usage(format!("position {position} is past the end of a video {length} seconds long"))
        }
        _ => Ok(()),
    }
}

/// Fails with `usage` unless `episode` names the film `details` describes, or an aired episode it
/// lists.
fn watchable(details: &TitleDetails, episode: Option<Episode>, now: i64) -> Result<(), Error> {
    let card = &details.card;
    let usage = |message: String| Err(Error::new(ErrorCode::Usage, message));
    let e = match (card.kind, episode) {
        (Kind::Film, None) => return Ok(()),
        (Kind::Film, Some(e)) => return usage(format!("{} is a film, so it has no {}", card.name, label(e))),
        (Kind::Series, None) => return usage(format!("{} is a series, so name one of its episodes", card.name)),
        (Kind::Series, Some(e)) => e,
    };
    match details.seasons.iter().flat_map(|s| &s.episodes).find(|info| info.episode == e) {
        None => usage(format!("{} is not an episode of {}", label(e), card.name)),
        Some(info) if info.aired.is_some_and(|aired| aired <= now) => Ok(()),
        Some(_) => usage(format!("{} has not aired yet", title_name(card, Some(e)))),
    }
}

#[derive(Deserialize)]
pub struct Library {
    title: TitleId,
    saved: bool,
}

/// `POST /api/library`: saves a title, or takes it out.
pub async fn library(ctx: &Ctx, request: Library) -> Result<Value, Error> {
    if request.saved {
        let details = ctx.titles.catalog.title(&request.title).await?;
        ctx.titles.shelf.save(&details.card, now())?;
    } else {
        ctx.titles.shelf.unsave(&request.title)?;
    }
    Ok(json!({ "saved": request.saved }))
}

/// A film or episode to go on with.
#[derive(Serialize)]
struct Continue {
    title: TitleCard,
    video: VideoRef,
    episode: Option<EpisodeInfo>,
    /// `None` when the video has not been started, as the episode after a finished one.
    resume_at: Option<f64>,
    duration: Option<f64>,
}

#[derive(Serialize)]
struct Home {
    /// The most recently watched first.
    #[serde(rename = "continue")]
    going_on: Vec<Continue>,
    saved: Vec<TitleCard>,
}

/// `GET /api/home`: for each title watched lately, its video to go on with, and the saved titles.
/// A finished film, and a series the person has caught up with, are left out.
pub async fn home(ctx: &Ctx) -> Result<Value, Error> {
    let now = now();
    let mut going_on = Vec::new();
    for recent in ctx.titles.shelf.recent(CONTINUE_MOST)? {
        let Ok(details) = ctx.titles.catalog.title(&recent.video.title).await else { continue };
        let watched = ctx.titles.shelf.watched(&recent.video.title)?;
        let Some(Next { video, resume_at }) = next(&details, &watched, now) else { continue };
        let episode = video
            .episode
            .and_then(|e| details.seasons.iter().flat_map(|s| &s.episodes).find(|info| info.episode == e).cloned());
        let duration = watched.iter().find(|w| w.video == video).and_then(|w| w.duration);
        going_on.push(Continue { title: details.card, video, episode, resume_at, duration });
    }
    super::value(Home { going_on, saved: ctx.titles.shelf.saved()? })
}
