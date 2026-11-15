//! The socket: the hello, then each request framed by length, and the streams that follow.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{broadcast, mpsc, oneshot};
use torrential_core::error::Details;
use torrential_core::protocol::{
    Event, GetOutcome, GetReply, METADATA_WAIT, PlayReply, Request, Response, TitleFrame, TitlePlayed, TitleRequest,
    WIRE_VERSION,
};
use torrential_core::{AttemptEvent, CastRequest, Error, ErrorCode, Player, Profile, SearchQuery, TorrentId};

use super::ctx::Ctx;
use super::downloads::{HEARTBEAT, files, get, listed, metadata, play, record, search, streams};
use super::owner::plays::Destination;
use super::owner::{Command, MetaTarget, Update};
use super::search_port::Progress;
use super::titles::attempt::{self, Containers, Outcome, PlayTitle, Played};
use super::titles::{home, pinned, sources, title_page};
use crate::log::log;
use crate::update::Version;
use crate::wire;

pub async fn handle(ctx: &Ctx, request: Request) -> Response {
    let owner = &ctx.owner;
    let result = match request {
        Request::Hello { .. } | Request::Watch { .. } => {
            Err(Error::new(ErrorCode::Usage, "hello and watch are not single requests"))
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
        Request::Pause { refs } => {
            owner.ask(|reply| Command::Pause { refs, reply }).await.map(|changes| Response::Changed { changes })
        }
        Request::Resume { refs, all } => {
            owner.ask(|reply| Command::Resume { refs, all, reply }).await.map(|changes| Response::Changed { changes })
        }
        Request::Remove { refs, delete } => owner
            .ask(|reply| Command::Remove { refs, delete, reply })
            .await
            .map(|changes| Response::Changed { changes }),
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
        Request::PlayTitle(request) => play_title(ctx, request, Destination::Plays, mpsc::unbounded_channel().0).await,
        Request::GetTitle(request) => {
            play_title(ctx, request, Destination::Folder(None), mpsc::unbounded_channel().0).await
        }
        Request::Devices => Ok(Response::Devices { list: ctx.devices.list(ctx.http_port) }),
        Request::AddDevice => {
            ctx.devices.add(ctx.http_port, torrential_core::time::now()).map(|list| Response::Devices { list })
        }
        Request::RemoveDevice { id } => ctx.devices.remove(&id, ctx.http_port).map(|list| Response::Devices { list }),
        Request::Tvs { fresh } => Ok(Response::Tvs { list: ctx.casts.search(fresh).await }),
        Request::Cast(request) => match ctx.casts.start(request).await {
            Ok((mut view, _)) => {
                super::cast::playing(&mut view).await.map(|cast| Response::Cast { cast: Box::new(cast) })
            }
            Err(error) => Err(error),
        },
        Request::CastControl { tv, action } => {
            ctx.casts.control(tv.as_deref(), action).await.map(|cast| Response::Cast { cast: Box::new(cast) })
        }
        Request::Home => home(ctx).await.map(|home| Response::Home { home: Box::new(home) }),
    };
    result.unwrap_or_else(|error| Response::Error { error })
}

async fn stream_search(ctx: &Ctx, query: SearchQuery, wr: &mut OwnedWriteHalf) -> std::io::Result<()> {
    let (progress_tx, mut progress) = mpsc::unbounded_channel();
    let Profile { preferences, player, .. } = query.profile;
    let rank_for = || ctx.machine.profile(preferences, player);
    let search = ctx.search.search_stream(query, &rank_for, progress_tx);
    tokio::pin!(search);
    let result = loop {
        tokio::select! {
            biased;
            Some(Progress { mut result, pending }) = progress.recv() => {
                record(ctx, &mut result);
                wire::write_async(wr, &Event::SearchUpdate { result: Box::new(result), pending }).await?;
            }
            result = &mut search => break result,
        }
    };
    let response = match result {
        Ok(mut result) => {
            record(ctx, &mut result);
            Response::Search { result }
        }
        Err(error) => Response::Error { error },
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
    let TitleRequest { video, source } = request;
    let pin = match source {
        Some(prefix) => Some(pinned(ctx, &video, &prefix).await?.id),
        None => None,
    };
    let jobs = ctx.owner.ask(|reply| Command::Status { refs: Vec::new(), reply }).await?;
    let before: BTreeSet<TorrentId> = jobs.iter().map(|job| job.id).collect();
    let ask =
        PlayTitle { video, pin, replace: None, player: Player::External, containers: Containers::Any, to: to.clone() };
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
            Some(step) = steps.recv() => wire::write_async(wr, &TitleFrame::Step { step: Box::new(step) }).await?,
            reply = &mut play => break reply,
            read = tokio::io::AsyncReadExt::read(rd, &mut probe) => match read {
                Ok(0) | Err(_) => return Ok(()),
                Ok(_) => {}
            },
        }
    };
    while let Ok(step) = steps.try_recv() {
        wire::write_async(wr, &TitleFrame::Step { step: Box::new(step) }).await?;
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
                Ok(step) => wire::write_async(wr, &TitleFrame::Step { step: Box::new(step) }).await?,
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break (&mut playing).await,
            },
            cast = &mut playing => break cast,
        }
    };
    let reply = reply.map_or_else(|error| Response::Error { error }, |cast| Response::Cast { cast: Box::new(cast) });
    wire::write_async(wr, &reply).await
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
        let _playing = match request {
            Request::Play(_) | Request::PlayTitle(_) => match ctx.playing.open() {
                Ok(open) => Some(open),
                Err(error) => {
                    wire::write_async(&mut wr, &Response::Error { error }).await?;
                    continue;
                }
            },
            _ => None,
        };
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
    if older(client, &ctx.version) {
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
    match ctx.owner.ask(|reply| Command::Restart { reply }).await {
        Ok(()) => {
            closed.keep();
            Response::Ok
        }
        Err(error) => Response::Error { error },
    }
}

fn older(client: &str, engine: &str) -> bool {
    Version::parse(engine).is_some_and(|engine| engine.newer_than(client))
}

async fn watch(ctx: &Ctx, refs: Vec<String>, mut rd: OwnedReadHalf, mut wr: OwnedWriteHalf) -> std::io::Result<()> {
    let everything = refs.is_empty();
    let (jobs, mut updates) = match ctx.owner.ask(|reply| Command::Watch { refs, reply }).await {
        Ok(watch) => watch,
        Err(error) => return wire::write_async(&mut wr, &Response::Error { error }).await,
    };
    let watched: Vec<_> = jobs.iter().map(|j| j.id).collect();
    wire::write_async(&mut wr, &Event::Snapshot { jobs }).await?;
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await;
    let mut probe = [0u8; 1];
    loop {
        let event = tokio::select! {
            update = updates.recv() => match update {
                Ok(Update::Job(job)) if everything || watched.contains(&job.id) => Event::Job { job },
                Ok(Update::Removed(id)) if everything || watched.contains(&id) => Event::Removed { id: id.to_hex() },
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let refs = if everything { Vec::new() } else { watched.iter().map(|id| id.to_hex()).collect() };
                    match ctx.owner.ask(|reply| Command::Status { refs, reply }).await {
                        Ok(jobs) => Event::Snapshot { jobs },
                        Err(_) => return Ok(()),
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            _ = heartbeat.tick() => Event::Heartbeat { at: torrential_core::time::now() },
            read = tokio::io::AsyncReadExt::read(&mut rd, &mut probe) => match read {
                Ok(0) | Err(_) => return Ok(()),
                Ok(_) => continue,
            },
        };
        heartbeat.reset();
        wire::write_async(&mut wr, &event).await?;
    }
}
