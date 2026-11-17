//! The requests of torrential, this user's and paired machines': each a POST to the engine's HTTP
//! port, answered in frames, with the updates of a request that streams before its reply.

use std::collections::BTreeSet;
use std::io;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot};
use torrential_core::error::Details;
use torrential_core::protocol::{
    Event, Frame, GetOutcome, GetReply, METADATA_WAIT, PlayReply, Request, Response, TitlePlayed, TitleRequest,
    TitleStep, WIRE_VERSION, encode_frame,
};
use torrential_core::{AttemptEvent, CastRequest, Error, ErrorCode, Player, Plays, SearchQuery, TorrentId};

use super::ctx::Ctx;
use super::devices::Asker;
use super::downloads::{
    Answer, JobEvents, Searching, files, get, listed, metadata, pause, play, remove, resume, search, streams,
};
use super::http::{Connection, Request as Http};
use super::owner::plays::Destination;
use super::owner::{Command, MetaTarget};
use super::search_port::Progress;
use super::titles::attempt::{self, Outcome, PlayTitle, Played};
use super::titles::{follow, home, pinned, sources, title_page};
use crate::log::log;
use crate::update::Version;
use crate::wire;

pub async fn handle(ctx: &Ctx, request: Request) -> Response {
    let owner = &ctx.owner;
    let result = match request {
        Request::Hello | Request::Watch { .. } | Request::PlayTitle(_) | Request::GetTitle(_) | Request::Cast(_) => {
            Err(Error::new(ErrorCode::Usage, "this request streams, so it is not a single request"))
        }
        Request::Search { query, .. } => search(ctx, query).await.map(|result| Response::Search { result }),
        Request::Files { target, timeout_ms } => {
            files(ctx, target, Duration::from_millis(timeout_ms.into())).await.map(|files| Response::Files { files })
        }
        Request::Get(request) => get(ctx, request).await.map(|reply| Response::Get { reply: Box::new(reply) }),
        Request::Play(request) => play(ctx, request).await.map(|reply| Response::Play { reply: Box::new(reply) }),
        Request::Status { refs } => {
            owner.ask(|reply| Command::Status { refs, reply }).await.map(|jobs| Response::Jobs { jobs })
        }
        Request::Pause { refs } => pause(ctx, refs).await.map(|changes| Response::Changed { changes }),
        Request::Resume { refs, all } => resume(ctx, refs, all).await.map(|changes| Response::Changed { changes }),
        Request::Remove { refs, delete } => {
            remove(ctx, refs, delete).await.map(|changes| Response::Changed { changes })
        }
        Request::Verify { reference, repair } => owner
            .ask(|reply| Command::Verify { reference, repair, reply })
            .await
            .map(|changes| Response::Changed { changes }),
        Request::Shutdown { .. } => {
            let (reply, done) = oneshot::channel();
            owner.tell(Command::Shutdown { reply });
            let _ = done.await;
            Ok(Response::Ok)
        }
        Request::Titles { words } => {
            ctx.titles.catalog.search(words.trim()).await.map(|titles| Response::Titles { titles })
        }
        Request::Title { id } => title_page(ctx, id).await.map(|page| Response::Title { title: Box::new(page) }),
        Request::Sources { video } => {
            sources(ctx, video).await.map(|sources| Response::Sources { sources: Box::new(sources) })
        }
        Request::Devices => Ok(Response::Devices { list: ctx.devices.list(ctx.http_port) }),
        Request::AddDevice => {
            ctx.devices.add(ctx.http_port, torrential_core::time::now()).map(|list| Response::Devices { list })
        }
        Request::RemoveDevice { id } => ctx.devices.remove(&id, ctx.http_port).map(|list| Response::Devices { list }),
        Request::Tvs { fresh } => Ok(Response::Tvs { list: ctx.casts.search(fresh).await }),
        Request::CastControl { tv, action } => {
            ctx.casts.control(tv.as_deref(), action).await.map(|cast| Response::Cast { cast: Box::new(cast) })
        }
        Request::Home => home(ctx).await.map(|home| Response::Home { home: Box::new(home) }),
        Request::Follow { video, release, socket } => {
            follow::follow(ctx.titles.watching.clone(), video, release, socket);
            Ok(Response::Ok)
        }
    };
    result.unwrap_or_else(|error| Response::Error { error })
}

/// Answers a request at `wire::PATH`, whose token the guard has checked, with its frames. The
/// query says the client's version and protocol. Every engine answers hello and shutdown, and
/// refuses the rest when the protocols differ. A paired device gets only the requests a device may
/// send. The connection closes after the reply.
pub async fn answer(conn: &mut Connection<'_>, http: &Http, asker: &Asker) -> io::Result<bool> {
    let ctx = conn.ctx;
    let speaks = http.param("wire").and_then(|wire| wire.parse::<u32>().ok());
    let (Some(version), Some(speaks)) = (http.param("version"), speaks) else {
        return conn.empty("400 Bad Request", "", false).await;
    };
    let request = serde_json::from_slice::<Request>(&http.body);
    conn.begin(wire::FRAMES, http).await?;
    if let (Asker::Device(device), Ok(request)) = (asker, &request)
        && let Some(error) = request.refused_to_device()
    {
        log(&format!("refused device {device} a request only this machine may send: {}", error.message));
        send(conn, &Response::Error { error }).await?;
        return conn.end().await;
    }
    let reply = match request {
        Ok(Request::Hello) => hello(ctx),
        Ok(Request::Shutdown { asked: false }) => switch(ctx, &version).await,
        Ok(request @ Request::Shutdown { asked: true }) => handle(ctx, request).await,
        _ if speaks != WIRE_VERSION => {
            let error = Error::new(
                ErrorCode::DaemonVersion,
                format!("the engine uses protocol version {WIRE_VERSION}, but this client uses {speaks}"),
            )
            .with_hint("run torrential engine stop, then run the command again")
            .with_next("torrential engine stop");
            Response::Error { error }
        }
        Err(e) => Response::Error { error: Error::internal(format!("the engine cannot read the request: {e}")) },
        Ok(Request::Watch { refs }) => {
            watch(conn, refs).await?;
            return conn.end().await;
        }
        Ok(Request::Search { query, stream: true }) => stream_search(conn, query).await?,
        Ok(Request::PlayTitle(request)) => title_frames(conn, request, Destination::Plays).await?,
        Ok(Request::GetTitle(request)) => title_frames(conn, request, Destination::Folder(None)).await?,
        Ok(Request::Cast(request)) => cast_frames(conn, request).await?,
        Ok(request) => handle(ctx, request).await,
    };
    send(conn, &reply).await?;
    conn.end().await
}

fn hello(ctx: &Ctx) -> Response {
    Response::Hello { engine_version: ctx.version.clone(), wire: WIRE_VERSION, pid: std::process::id() }
}

/// Sends one frame.
async fn send(conn: &mut Connection<'_>, message: &impl Serialize) -> io::Result<()> {
    conn.chunk(&encode_frame(message).map_err(io::Error::other)?).await
}

/// Sends the results as each source answers, and returns the whole search.
async fn stream_search(conn: &mut Connection<'_>, query: SearchQuery) -> io::Result<Response> {
    let mut search = Searching::start(conn.ctx, query);
    loop {
        match search.next().await {
            Answer::SoFar(Progress { result, pending }) => {
                send(conn, &Event::SearchUpdate { result: Box::new(result), pending }).await?;
            }
            Answer::Done(Ok(result)) => return Ok(Response::Search { result }),
            Answer::Done(Err(error)) => return Ok(Response::Error { error }),
        }
    }
}

/// Plays a film or an episode into `to` from the first of its sources that starts, sending each
/// step to `steps`. A play into the plays folder answers where the video and its subtitles are
/// served, as `play` answers, and a play into a folder answers as `get` answers.
async fn play_title(
    ctx: &Ctx,
    request: TitleRequest,
    to: Destination,
    steps: mpsc::UnboundedSender<AttemptEvent>,
) -> Result<Response, Error> {
    let _playing = ctx.playing.open()?;
    let TitleRequest { video, source } = request;
    let pin = match source {
        Some(prefix) => Some(pinned(ctx, &video, &prefix).await?.id),
        None => None,
    };
    let jobs = ctx.owner.ask(|reply| Command::Status { refs: Vec::new(), reply }).await?;
    let before: BTreeSet<TorrentId> = jobs.iter().map(|job| job.id).collect();
    let ask = PlayTitle { video, pin, replace: None, player: Player::External, plays: Plays::ANY, to: to.clone() };
    let played = match attempt::run(ctx, ask, |step| drop(steps.send(step))).await {
        Outcome::Played(played) => *played,
        Outcome::Failed { error, tried } if tried.is_empty() => return Err(error),
        Outcome::Failed { error, tried } => return Err(error.with_details(Details::Tried(tried))),
    };
    let Played { name, video, release, job: id, file, resume_at, tried } = played;
    let title = Box::new(TitlePlayed { video, name, release, tried, resume_at });
    let mut jobs = ctx.owner.ask(|reply| Command::Status { refs: vec![id.to_hex()], reply }).await?;
    let job = jobs.pop().ok_or_else(|| Error::new(ErrorCode::NotFound, "the download that played is gone"))?;
    match to {
        Destination::Plays => {
            let info = metadata(ctx, MetaTarget::Job(id), Instant::now() + METADATA_WAIT).await?;
            let playable = crate::playback::stream::around(&listed(ctx, &info).await, file)
                .ok_or_else(|| Error::internal(format!("the download has no file {file}")))?;
            let files = [playable.video].into_iter().chain(playable.subtitles).collect();
            let streams = streams(ctx, id, &info.metainfo, files);
            Ok(Response::PlayTitle { reply: Box::new(PlayReply { job, streams }), title })
        }
        Destination::Folder(_) => {
            let outcome = if before.contains(&id) { GetOutcome::Existing } else { GetOutcome::Started };
            let reply = GetReply { outcome, job, plan: None, previous_selection: None };
            Ok(Response::GetTitle { reply: Box::new(reply), title })
        }
    }
}

/// A title play: a frame for each step, and returns the reply. The play stops, and the source
/// under test is given up, if the client goes away first.
async fn title_frames(conn: &mut Connection<'_>, request: TitleRequest, to: Destination) -> io::Result<Response> {
    let (steps_tx, mut steps) = mpsc::unbounded_channel();
    let play = play_title(conn.ctx, request, to, steps_tx);
    tokio::pin!(play);
    let reply = loop {
        tokio::select! {
            biased;
            Some(step) = steps.recv() => send(conn, &Frame::Update(TitleStep { step: Box::new(step) })).await?,
            reply = &mut play => break reply,
            gone = conn.gone() => return Err(gone),
        }
    };
    while let Ok(step) = steps.try_recv() {
        send(conn, &Frame::Update(TitleStep { step: Box::new(step) })).await?;
    }
    Ok(reply.unwrap_or_else(|error| Response::Error { error }))
}

/// A cast: a frame for each step of finding a title's source, and returns the cast once the TV
/// plays it. The cast goes on after the client leaves.
async fn cast_frames(conn: &mut Connection<'_>, request: CastRequest) -> io::Result<Response> {
    let (mut view, mut steps) = match conn.ctx.casts.start(request).await {
        Ok(started) => started,
        Err(error) => return Ok(Response::Error { error }),
    };
    let playing = super::cast::playing(&mut view);
    tokio::pin!(playing);
    let reply = loop {
        tokio::select! {
            biased;
            step = steps.recv() => match step {
                Ok(step) => send(conn, &Frame::Update(TitleStep { step: Box::new(step) })).await?,
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break (&mut playing).await,
            },
            cast = &mut playing => break cast,
        }
    };
    Ok(reply.map_or_else(|error| Response::Error { error }, |cast| Response::Cast { cast: Box::new(cast) }))
}

/// Answers a stop that a client sends without the person asking, to run its own version instead. An
/// engine newer than the client keeps running, since torrential 0.4 sends this stop to an engine of
/// any other version. Any other engine stops only when nothing plays and every download is saved.
async fn switch(ctx: &Ctx, client: &str) -> Response {
    if Version::newer(&ctx.version, client) {
        log(&format!("torrential {client} asked the engine to stop for its own version"));
        let error = Error::new(
            ErrorCode::DaemonVersion,
            format!("the engine runs the newer torrential {}, so it keeps running", ctx.version),
        )
        .with_hint("restart this torrential");
        return Response::Error { error };
    }
    let Some(closed) = ctx.playing.try_close() else {
        let error = Error::new(ErrorCode::DaemonVersion, format!("torrential switches to {client} when the film ends"));
        return Response::Error { error };
    };
    match super::switch::stop_for_next(ctx, closed).await {
        Ok(()) => Response::Ok,
        Err(error) => Response::Error { error },
    }
}

/// Sends the downloads `refs` names, then each change to them, until the client goes away.
async fn watch(conn: &mut Connection<'_>, refs: Vec<String>) -> io::Result<()> {
    let (mut events, snapshot) = match JobEvents::start(conn.ctx, refs).await {
        Ok(started) => started,
        Err(error) => return send(conn, &Response::Error { error }).await,
    };
    send(conn, &snapshot).await?;
    loop {
        tokio::select! {
            event = events.next() => match event {
                Some(event) => send(conn, &event).await?,
                None => return Ok(()),
            },
            gone = conn.gone() => return Err(gone),
        }
    }
}
