//! The socket: the hello, then each request framed by length, and the streams that follow.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use torrential_core::error::Details;
use torrential_core::protocol::{
    Event, Frame, GetOutcome, GetReply, METADATA_WAIT, PlayReply, Request, Response, TitlePlayed, TitleRequest,
    TitleStep, WIRE_VERSION,
};
use torrential_core::{AttemptEvent, CastRequest, Error, ErrorCode, Player, Plays, SearchQuery, TorrentId};

use super::ctx::Ctx;
use super::downloads::{
    Answer, JobEvents, Searching, files, get, listed, metadata, pause, play, remove, resume, search, streams,
};
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
        Request::Hello { .. }
        | Request::Watch { .. }
        | Request::PlayTitle(_)
        | Request::GetTitle(_)
        | Request::Cast(_) => Err(Error::new(ErrorCode::Usage, "this request streams, so it is not a single request")),
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

async fn stream_search(ctx: &Ctx, query: SearchQuery, wr: &mut OwnedWriteHalf) -> std::io::Result<()> {
    let mut search = Searching::start(ctx, query);
    let response = loop {
        match search.next().await {
            Answer::SoFar(Progress { result, pending }) => {
                wire::write_async(wr, &Event::SearchUpdate { result: Box::new(result), pending }).await?;
            }
            Answer::Done(Ok(result)) => break Response::Search { result },
            Answer::Done(Err(error)) => break Response::Error { error },
        }
    };
    wire::write_async(wr, &response).await
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
            let playable = torrential_core::stream::around(&listed(ctx, &info).await, file)
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

/// A title play over the socket: a frame for each step, then the reply. The play stops, and the
/// source under test is given up, if the client goes away first.
async fn title_frames(
    ctx: &Ctx,
    request: TitleRequest,
    to: Destination,
    rd: &mut OwnedReadHalf,
    wr: &mut OwnedWriteHalf,
) -> std::io::Result<()> {
    let (steps_tx, mut steps) = mpsc::unbounded_channel();
    let play = play_title(ctx, request, to, steps_tx);
    tokio::pin!(play);
    let mut probe = [0u8; 1];
    let reply = loop {
        tokio::select! {
            biased;
            Some(step) = steps.recv() => wire::write_async(wr, &Frame::Update(TitleStep { step: Box::new(step) })).await?,
            reply = &mut play => break reply,
            read = tokio::io::AsyncReadExt::read(rd, &mut probe) => match read {
                Ok(0) | Err(_) => return Ok(()),
                Ok(_) => {}
            },
        }
    };
    while let Ok(step) = steps.try_recv() {
        wire::write_async(wr, &Frame::Update(TitleStep { step: Box::new(step) })).await?;
    }
    wire::write_async(wr, &reply.unwrap_or_else(|error| Response::Error { error })).await
}

/// A cast over the socket: a frame for each step of finding a title's source, then the cast once
/// the TV plays it. The cast goes on after the client leaves.
async fn cast_frames(ctx: &Ctx, request: CastRequest, wr: &mut OwnedWriteHalf) -> std::io::Result<()> {
    let (mut view, mut steps) = match ctx.casts.start(request).await {
        Ok(started) => started,
        Err(error) => return wire::write_async(wr, &Response::Error { error }).await,
    };
    let playing = super::cast::playing(&mut view);
    tokio::pin!(playing);
    let reply = loop {
        tokio::select! {
            biased;
            step = steps.recv() => match step {
                Ok(step) => wire::write_async(wr, &Frame::Update(TitleStep { step: Box::new(step) })).await?,
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break (&mut playing).await,
            },
            cast = &mut playing => break cast,
        }
    };
    let reply = reply.map_or_else(|error| Response::Error { error }, |cast| Response::Cast { cast: Box::new(cast) });
    wire::write_async(wr, &reply).await
}

/// Answers each connection to `listener` from this user, until the listener fails.
pub async fn accept(listener: UnixListener, ctx: Arc<Ctx>) {
    let uid = crate::paths::uid();
    while let Ok((stream, _)) = listener.accept().await {
        if stream.peer_cred().is_ok_and(|cred| cred.uid() == uid) {
            tokio::spawn(connection(ctx.clone(), stream));
        }
    }
}

pub async fn connection(ctx: Arc<Ctx>, stream: UnixStream) {
    let _ = serve(&ctx, stream).await;
}

async fn serve(ctx: &Ctx, stream: UnixStream) -> std::io::Result<()> {
    let (mut rd, mut wr) = stream.into_split();
    let Some(Request::Hello { client_version, wire: client_wire }) = wire::read_async::<Request>(&mut rd).await? else {
        return Ok(());
    };
    let hello = Response::Hello {
        engine_version: ctx.version.clone(),
        wire: WIRE_VERSION,
        pid: std::process::id(),
        http_port: ctx.http_port,
    };
    wire::write_async(&mut wr, &hello).await?;
    while let Some(request) = wire::read_async::<Request>(&mut rd).await? {
        if request == (Request::Shutdown { asked: false }) {
            wire::write_async(&mut wr, &switch(ctx, &client_version).await).await?;
            continue;
        }
        if client_wire != WIRE_VERSION && !matches!(request, Request::Shutdown { .. }) {
            let error = Error::new(
                ErrorCode::DaemonVersion,
                format!("the engine uses protocol version {WIRE_VERSION}, but this client uses {client_wire}"),
            )
            .with_hint("run torrential engine stop, then run the command again")
            .with_next("torrential engine stop");
            wire::write_async(&mut wr, &Response::Error { error }).await?;
            continue;
        }
        match request {
            Request::Watch { refs } => return watch(ctx, refs, rd, wr).await,
            Request::Search { query, stream: true } => stream_search(ctx, query, &mut wr).await?,
            Request::PlayTitle(request) => title_frames(ctx, request, Destination::Plays, &mut rd, &mut wr).await?,
            Request::GetTitle(request) => {
                title_frames(ctx, request, Destination::Folder(None), &mut rd, &mut wr).await?
            }
            Request::Cast(request) => cast_frames(ctx, request, &mut wr).await?,
            request => {
                let response = handle(ctx, request).await;
                wire::write_async(&mut wr, &response).await?;
            }
        }
    }
    Ok(())
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

async fn watch(ctx: &Ctx, refs: Vec<String>, mut rd: OwnedReadHalf, mut wr: OwnedWriteHalf) -> std::io::Result<()> {
    let (mut events, snapshot) = match JobEvents::start(ctx, refs).await {
        Ok(started) => started,
        Err(error) => return wire::write_async(&mut wr, &Response::Error { error }).await,
    };
    wire::write_async(&mut wr, &snapshot).await?;
    let mut probe = [0u8; 1];
    loop {
        let event = tokio::select! {
            event = events.next() => match event {
                Some(event) => event,
                None => return Ok(()),
            },
            read = tokio::io::AsyncReadExt::read(&mut rd, &mut probe) => match read {
                Ok(0) | Err(_) => return Ok(()),
                Ok(_) => continue,
            },
        };
        wire::write_async(&mut wr, &event).await?;
    }
}
