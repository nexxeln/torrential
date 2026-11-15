//! Films and series for the web app: finding one, its page, its pictures, its sources, playing
//! it, where the person stopped, and what they saved.

use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use torrential_core::catalog::Episode;
use torrential_core::time::now;
use torrential_core::{
    ArtKind, AttemptEvent, Decoders, Decoding, Error, ErrorCode, Playback, Player, Plays, Release, ReleaseId,
    TitleCard, TitleId, TorrentId, VideoRef, Viewer, Watched,
};

use super::super::ctx::Ctx;
use super::super::devices::Asker;
use super::super::http::{Connection, Request};
use super::super::owner::plays::Destination;
use super::super::owner::{Command, Shelving};
use super::super::titles;
use super::super::titles::attempt::{self, Outcome, PlayTitle, Played};
use super::super::titles::watching::Screen;
use super::{NDJSON, fail, line, number, required, value};

/// How long a browser keeps a picture. A title's pictures rarely change.
const ART_CACHE: &str = "Cache-Control: private, max-age=2592000\r\n";

/// `GET /api/titles?q=`: films and series named like the words. With title lookups off in
/// config.yaml it finds none, and says it is offline.
pub async fn search(ctx: &Ctx, request: &Request) -> Result<Value, Error> {
    let words = required(request, "q")?;
    if !ctx.titles.catalog.online() {
        return value(Found { titles: Vec::new(), offline: true });
    }
    value(Found { titles: ctx.titles.catalog.search(words.trim()).await?, offline: false })
}

/// What `GET /api/titles` answers.
#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "TitleSearch"))]
pub(super) struct Found {
    titles: Vec<TitleCard>,
    /// Title lookups are off in config.yaml.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[cfg_attr(test, ts(as = "Option<bool>", optional))]
    offline: bool,
}

/// `GET /api/title?id=`.
pub async fn page(ctx: &Ctx, request: &Request) -> Result<Value, Error> {
    let id: TitleId = required(request, "id")?.parse()?;
    super::value(titles::title_page(ctx, id).await?)
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
        Err(error) => conn.json("404 Not Found", &super::ErrorBody { error: &error }, request).await,
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
        let query = ctx.titles.query(&title, (ctx.preferences)(), player).await?;
        Ok::<_, Error>((query, title.video, jobs))
    };
    let asked = tokio::select! {
        asked = asked => asked,
        gone = conn.gone() => return Err(gone),
    };
    match asked {
        Ok((query, video, jobs)) => {
            let (player, ffmpeg) = (query.profile.player, ctx.media.ffmpeg.converts());
            let line = |releases| ctx.titles.line(&jobs, &video, releases, player, ffmpeg);
            super::results(conn, request, query, line).await
        }
        Err(error) => fail(conn, request, &error).await,
    }
}

/// The screen that pressed play and what its player decodes, as the web app measures them.
#[derive(Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "Viewer"))]
pub(super) struct Measured {
    /// The screen in device pixels.
    width: u32,
    height: u32,
    /// Device pixels to a CSS pixel.
    scale: f64,
    playback: Playback,
    /// How the browser decodes each codec, from its `mediaCapabilities`.
    #[serde(default)]
    #[cfg_attr(test, ts(optional))]
    decoders: Option<Decoders>,
}

impl Measured {
    fn viewer(self) -> Result<Viewer, Error> {
        let Measured { width, height, scale, playback, decoders } = self;
        let usage = |message: String| Err(Error::new(ErrorCode::Usage, message));
        if width == 0 || height == 0 {
            return usage(format!("a {width}x{height} screen has no pixels"));
        }
        if !(scale.is_finite() && scale > 0.0) {
            return usage(format!("scale {scale} is not a number of device pixels to a CSS pixel"));
        }
        Ok(Viewer { decoders, ..Viewer::measured(width, height, scale, playback) })
    }
}

/// Who a source listing ranks for, from its `width`, `height`, `scale`, and `playback`
/// parameters, and `h264`, `hevc` and `av1` for how the browser decodes each. The screen's three
/// come with `playback` or not at all, and the codecs all three or none.
fn listed_for(request: &Request, asker: &Asker) -> Result<Player, Error> {
    let size = (number(request, "width")?, number(request, "height")?, number(request, "scale")?);
    let playback = request.param("playback").map(|text| playback(&text)).transpose()?;
    let decoding = |codec: &str| request.param(codec).map(|text| decoding(&text)).transpose();
    let decoders = match (decoding("h264")?, decoding("hevc")?, decoding("av1")?) {
        (Some(h264), Some(hevc), Some(av1)) => Some(Decoders { h264, hevc, av1 }),
        (None, None, None) => None,
        _ => return Err(Error::new(ErrorCode::Usage, "how a browser decodes needs all of h264, hevc, and av1")),
    };
    let viewer = match (size, playback) {
        ((Some(width), Some(height), Some(scale)), Some(playback)) => {
            Some(Measured { width, height, scale, playback, decoders }.viewer()?)
        }
        ((None, None, None), _) => None,
        _ => return Err(Error::new(ErrorCode::Usage, "a viewer needs all of width, height, scale, and playback")),
    };
    Ok(player(viewer, playback, asker))
}

/// Who a web play or source listing ranks for. Every browser decodes what it reports. A browser
/// on this Mac shows this Mac's display, so a paired device's screen counts, and this one's only
/// on a machine without a display. A tab opened before browsers reported their screen reports
/// only what it decodes. A request that reports neither ranks as `torrential play` does.
fn player(viewer: Option<Viewer>, playback: Option<Playback>, asker: &Asker) -> Player {
    match (viewer, playback) {
        (Some(viewer), _) if *asker != Asker::Local => Player::Device(viewer),
        (Some(Viewer { screen, playback, decoders }), _) => {
            Player::Browser { playback, measured: Some(screen), decoders }
        }
        (None, Some(playback)) => Player::Browser { playback, measured: None, decoders: None },
        (None, None) => Player::External,
    }
}

fn decoding(text: &str) -> Result<Decoding, Error> {
    serde_json::from_value(Value::String(text.to_string())).map_err(|_| {
        let words: Vec<&str> = Decoding::ALL.iter().map(|decoding| decoding.as_str()).collect();
        Error::new(ErrorCode::Usage, format!("{text} is not a decoding: use {}", words.join(", ")))
    })
}

fn playback(text: &str) -> Result<Playback, Error> {
    serde_json::from_value(Value::String(text.to_string())).map_err(|_| {
        let words: Vec<&str> = Playback::ALL.iter().map(|playback| playback.as_str()).collect();
        Error::new(ErrorCode::Usage, format!("{text} is not a playback: use {}", words.join(", ")))
    })
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "PlayTitle", optional_fields))]
pub(super) struct Play {
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
    let _playing = match ctx.playing.open() {
        Ok(open) => open,
        Err(updating) => return fail(conn, request, &updating).await,
    };
    let ask = super::body::<Play>(request).and_then(|Play { video, viewer, playback, source, replace }| {
        let player = player(viewer.map(Measured::viewer).transpose()?, playback, asker);
        Ok(PlayTitle { video, pin: source, replace, player, plays: Plays::BROWSER, to: Destination::Plays })
    });
    let ask = match ask {
        Ok(ask) => ask,
        Err(error) => return fail(conn, request, &error).await,
    };
    conn.begin(NDJSON, request).await?;
    let (events_tx, mut events) = mpsc::unbounded_channel();
    // The last line comes from how the play ended.
    let attempt = attempt::run(ctx, ask, move |event| {
        if !matches!(event, AttemptEvent::Playing { .. } | AttemptEvent::Failed { .. }) {
            let _ = events_tx.send(event);
        }
    });
    tokio::pin!(attempt);
    let outcome = loop {
        tokio::select! {
            biased;
            Some(event) = events.recv() => line(conn, &event).await?,
            outcome = &mut attempt => break outcome,
            gone = conn.gone() => return Err(gone),
        }
    };
    while let Ok(event) = events.try_recv() {
        line(conn, &event).await?;
    }
    line(conn, &last(ctx, outcome).await).await?;
    conn.end().await
}

/// The last line of a title play as the web app reads it. Once a source plays, the player gets
/// where the video is served in place of the download and file that play.
pub(in crate::daemon) async fn last(ctx: &Ctx, outcome: Outcome) -> Value {
    let (error, tried) = match outcome {
        Outcome::Played(played) => {
            let Played { video, release, job, file, resume_at, tried, .. } = *played;
            match super::play::playing_file(ctx, job, file).await {
                Ok(play) => {
                    let line = PlayingLine::Playing { video, release, play, resume_at };
                    return serde_json::to_value(line).expect("steps serialize");
                }
                Err(error) => (error, tried),
            }
        }
        Outcome::Failed { error, tried } => (error, tried),
    };
    serde_json::to_value(AttemptEvent::Failed { error, tried }).expect("steps serialize")
}

/// The `playing` line of a title play: the video, and where the player gets it.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(super) enum PlayingLine {
    Playing { video: VideoRef, release: Release, play: super::play::Playing, resume_at: Option<f64> },
}

/// What `POST /api/watched` takes.
#[derive(Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Position {
    video: VideoRef,
    /// Seconds.
    position: f64,
    duration: Option<f64>,
    release: Option<ReleaseId>,
    /// A word the tab made up for the video it plays. A tab that sends none is one tab.
    #[serde(default)]
    tab: String,
    /// The tab started or resumed the video since its last save, so it keeps the place.
    #[serde(default)]
    starts: bool,
}

/// `POST /api/watched`: where a film or episode stopped. The player sends it every few seconds,
/// and when it starts, resumes or pauses. `kept` is false while another screen that started or
/// resumed the video since keeps its place.
pub async fn watched(ctx: &Ctx, request: Position) -> Result<Value, Error> {
    let Position { video, position, duration, release, tab, starts } = request;
    check_times(position, duration)?;
    let now = now();
    titles::playable(&ctx.titles.catalog.title(&video.title).await?, video.episode, now)?;
    let screen = Screen::Tab(tab);
    if starts {
        ctx.titles.watching.start(&screen, &video);
    }
    let watched = Watched { video, position, duration, release, at: now };
    let kept = ctx.titles.watching.save(&screen, &watched)?;
    value(Saved { watched, kept })
}

/// What `POST /api/watched` answers.
#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(super) struct Saved {
    watched: Watched,
    /// False while another screen keeps the place.
    kept: bool,
}

/// What `POST /api/watched/forget` takes.
#[derive(Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Forget {
    video: VideoRef,
}

/// `POST /api/watched/forget`: forgets where a film or episode stopped, so it plays from the
/// beginning and leaves Continue.
pub async fn forget(ctx: &Ctx, request: Forget) -> Result<Value, Error> {
    ctx.titles.watching.forget(&request.video)?;
    Ok(serde_json::json!({}))
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

#[derive(Deserialize)]
pub struct Library {
    title: TitleId,
    saved: bool,
}

/// What `POST /api/library` answers.
#[derive(Serialize)]
struct Shelved {
    saved: bool,
}

/// `POST /api/library`: saves a title, or takes it out.
pub async fn library(ctx: &Ctx, request: Library) -> Result<Value, Error> {
    let shelving = match request.saved {
        true => Shelving::Save(ctx.titles.catalog.title(&request.title).await?.card),
        false => Shelving::Unsave(request.title),
    };
    ctx.owner.ask(|reply| Command::Library { shelving, reply }).await?;
    value(Shelved { saved: request.saved })
}

/// `GET /api/home`: for each title watched lately, its video to go on with, and the saved titles.
pub async fn home(ctx: &Ctx) -> Result<Value, Error> {
    super::value(titles::home(ctx).await?)
}
