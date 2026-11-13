//! Play on TV. The engine finds the TVs on the home network with SSDP, and plays a film, an
//! episode, or a torrent's video on one over UPnP AV: it mints a cast grant for the file, tells
//! the TV its URL, and asks the TV where it is every second while it plays, which goes to the
//! watch history. The grant is dropped once the cast ends, and the TV can fetch nothing more.

pub mod didl;
mod ssdp;
mod upnp;
mod xml;

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use torrential_core::text::sanitize;
use torrential_core::title::Span;
use torrential_core::{
    AttemptEvent, Cast, CastAction, CastRequest, CastState, Container, Error, ErrorCode, ReleaseId, Target, TitleLink,
    TorrentId, Tv, TvList, VideoRef, Watched,
};

use super::devices::names::Names;
use super::owner::{Finish, MetaTarget, now};
use super::server::{self, Ctx, METADATA_WAIT};
use super::titles::attempt::{self, Containers, PlayTitle};
use super::web::{play, watch as web_watch};
use crate::output::views::title_name;
pub use ssdp::Find;
use ssdp::Heard;
use upnp::{Failed, Renderer, Service};

/// A search this recent answers a request for the TVs without searching again.
const FRESH: Duration = Duration::from_secs(30);
/// A TV that misses this many searches in a row leaves the list.
const MISSES: u8 = 2;
/// How long a device description may take to arrive.
const DESCRIBE_WITHIN: Duration = Duration::from_secs(4);
/// How many devices a search describes at once.
const DESCRIBING: usize = 4;
/// How often a cast asks the TV where it is.
const POLL: Duration = Duration::from_secs(1);
/// How often a cast asks the TV its volume, in polls.
const VOLUME_EVERY: u32 = 5;
/// How often the position goes to the watch history, as the web player saves it.
const SAVE_EVERY: Duration = Duration::from_secs(5);
/// A position this close to the start is not kept, as in the web player.
const WORTH_SAVING: f64 = 5.0;
/// How long a TV may take to start playing once it has the URL. The first bytes are in by then.
const START_WITHIN: Duration = Duration::from_secs(90);
/// A TV that answers nothing for this long has gone.
const LOST_AFTER: Duration = Duration::from_secs(15);
/// How long an ended cast stays listed, so whoever started it can see how it ended.
const ENDED_KEPT: Duration = Duration::from_secs(60);
/// How long the engine waits to be reachable on the home network before it gives up on a cast.
const REACHABLE_WITHIN: Duration = Duration::from_secs(5);
/// Tries at the seek that resumes a video, since some TVs refuse one while they start.
const RESUME_TRIES: u32 = 3;

/// The TVs on the home network, and a cast for each TV that plays one.
pub struct Casts {
    ctx: Weak<Ctx>,
    find: Find,
    http: reqwest::Client,
    state: Mutex<State>,
    /// Held while a search runs, so requests that arrive meanwhile take its answer.
    searching: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct State {
    /// By ID, so a device that copies a TV's UDN from another address is another TV.
    tvs: BTreeMap<String, Known>,
    searched: Option<Instant>,
    /// By the TV's ID.
    casts: BTreeMap<String, Entry>,
    /// How each file a cast grant serves goes to the TV, by the grant's path.
    served: HashMap<String, Dlna>,
    next: u64,
}

struct Known {
    renderer: Arc<Renderer>,
    location: String,
    missed: u8,
}

impl State {
    /// Takes in the renderers one search found. A TV that misses `MISSES` searches in a row
    /// leaves the list, unless a cast plays on it, and one that missed the last search leaves to
    /// keep the list to `ssdp::MOST`.
    fn found(&mut self, seen: Vec<Known>) {
        let seen: BTreeMap<String, Known> = seen.into_iter().map(|k| (k.renderer.tv.id.clone(), k)).collect();
        let State { tvs, casts, .. } = self;
        for (id, known) in tvs.iter_mut() {
            if !seen.contains_key(id) {
                known.missed = known.missed.saturating_add(1);
            }
        }
        tvs.retain(|id, known| known.missed < MISSES || casts.contains_key(id));
        tvs.extend(seen);
        let mut missing: Vec<String> = tvs
            .iter()
            .filter(|(id, known)| known.missed > 0 && !casts.contains_key(*id))
            .map(|(id, _)| id.clone())
            .collect();
        while tvs.len() > ssdp::MOST
            && let Some(id) = missing.pop()
        {
            tvs.remove(&id);
        }
        self.searched = Some(Instant::now());
    }

    /// The TVs by name.
    fn tvs(&self) -> Vec<Tv> {
        let mut tvs: Vec<Tv> = self.tvs.values().map(|known| known.renderer.tv.clone()).collect();
        tvs.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
        tvs
    }
}

/// A cast, and the way to its task.
struct Entry {
    id: u64,
    view: watch::Receiver<Cast>,
    control: mpsc::UnboundedSender<Control>,
}

enum Control {
    Act(CastAction, oneshot::Sender<Result<Cast, Error>>),
    /// Another cast takes the TV: keep the position, and end without stopping the TV.
    Leave,
}

/// How a file a cast grant serves goes to the TV: its type in the TV's spelling, and the DLNA
/// headers TVs look for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Dlna {
    pub mime: String,
    /// The subtitle's URL, which Samsung TVs read from a header of the video's answer.
    caption: Option<String>,
    video: bool,
}

impl Dlna {
    pub fn headers(&self) -> String {
        let mut headers = if self.video {
            format!("transferMode.dlna.org: Streaming\r\ncontentFeatures.dlna.org: {}\r\n", didl::FEATURES)
        } else {
            "transferMode.dlna.org: Interactive\r\n".to_string()
        };
        if let Some(caption) = &self.caption {
            headers.push_str(&format!("CaptionInfo.sec: {caption}\r\n"));
        }
        headers
    }
}

impl Casts {
    pub fn new(ctx: Weak<Ctx>, find: Find) -> Self {
        // A redirect could lead anywhere, and a TV has no use for one.
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .build()
            .expect("a plain HTTP client builds");
        Self { ctx, find, http, state: Mutex::default(), searching: tokio::sync::Mutex::new(()) }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The TVs and the casts as known now.
    pub fn list(&self) -> TvList {
        let state = self.state();
        let mut casts: Vec<Cast> = state.casts.values().map(|entry| entry.view.borrow().clone()).collect();
        casts.sort_by_key(|cast| cast.started_at);
        TvList { tvs: state.tvs(), casts }
    }

    /// The TVs and the casts, after a search of the home network unless one ran in the last 30
    /// seconds and `fresh` is false.
    pub async fn search(&self, fresh: bool) -> TvList {
        let before = self.state().searched;
        let _searching = self.searching.lock().await;
        let searched = self.state().searched;
        let recent = searched.is_some_and(|at| at.elapsed() < FRESH);
        if searched != before || (recent && !fresh) {
            return self.list();
        }
        let answers = ssdp::search(self.find, &Names::read().lan).await.unwrap_or_default();
        let known: HashMap<String, Arc<Renderer>> = {
            let state = self.state();
            state.tvs.values().map(|k| (k.location.clone(), k.renderer.clone())).collect()
        };
        let mut found = tokio::task::JoinSet::new();
        let describing = Arc::new(tokio::sync::Semaphore::new(DESCRIBING));
        for heard in answers {
            let (http, known) = (self.http.clone(), known.get(heard.location.as_str()).cloned());
            let describing = describing.clone();
            found.spawn(async move {
                let _turn = describing.acquire_owned().await;
                let renderer = match known {
                    Some(renderer) => Some(renderer),
                    None => renderer(&http, &heard).await.map(Arc::new),
                };
                renderer.map(|renderer| (heard.location.to_string(), renderer))
            });
        }
        let mut seen = Vec::new();
        while let Some(answer) = found.join_next().await {
            if let Ok(Some((location, renderer))) = answer {
                seen.push(Known { renderer, location, missed: 0 });
            }
        }
        self.state().found(seen);
        self.list()
    }

    /// Starts a cast in place of any on the same TV. Returns the cast as it changes, and the steps
    /// of finding a title's source.
    pub async fn start(
        &self,
        request: CastRequest,
    ) -> Result<(watch::Receiver<Cast>, broadcast::Receiver<AttemptEvent>), Error> {
        let ctx = self.ctx.upgrade().ok_or_else(|| Error::internal("the engine is stopping"))?;
        if request.start_at.is_some_and(|at| !at.is_finite() || at < 0.0) {
            return Err(Error::new(ErrorCode::Usage, "start_at is not a time in a video"));
        }
        let tv = match self.choose(request.tv.as_deref()) {
            Err(error) if error.code == ErrorCode::NotFound => {
                self.search(true).await;
                self.choose(request.tv.as_deref())?
            }
            chosen => chosen?,
        };
        // A title's name comes once its lookup answers, and a torrent's once its file is chosen.
        let (name, state) = match &request.target {
            Target::Title(video) => (video.to_string(), CastState::Finding),
            _ => ("the video".to_string(), CastState::Loading),
        };
        let cast = Cast {
            tv: tv.tv.id.clone(),
            tv_name: tv.tv.name.clone(),
            name,
            video: None,
            download: None,
            file: None,
            subtitle: None,
            state,
            position: None,
            duration: None,
            volume: None,
            started_at: now(),
        };
        let (view_tx, view) = watch::channel(cast);
        let (steps_tx, steps) = broadcast::channel(64);
        let (control_tx, control) = mpsc::unbounded_channel();
        let (id, before) = {
            let mut state = self.state();
            state.next += 1;
            let id = state.next;
            let entry = Entry { id, view: view.clone(), control: control_tx };
            let before = state.casts.insert(tv.tv.id.clone(), entry).map(|old| {
                let _ = old.control.send(Control::Leave);
                old.view
            });
            (id, before)
        };
        let run = Run { ctx, tv, view: view_tx, id, before };
        tokio::spawn(run.cast(request, control, steps_tx));
        Ok((view, steps))
    }

    /// Asks the cast on the TV `tv` names, or the only cast playing, to pause, resume, seek, stop,
    /// or set the volume, and returns it after.
    pub async fn control(&self, tv: Option<&str>, action: CastAction) -> Result<Cast, Error> {
        let control = {
            let state = self.state();
            let live: Vec<&Entry> = state.casts.values().filter(|entry| !entry.view.borrow().ended()).collect();
            let chosen = match tv {
                None => match live.as_slice() {
                    [one] => *one,
                    [] => return Err(nothing_plays()),
                    many => {
                        let names: Vec<String> = many
                            .iter()
                            .map(|e| {
                                let cast = e.view.borrow();
                                at_address(&cast.tv_name, &cast.tv)
                            })
                            .collect();
                        return Err(Error::new(
                            ErrorCode::AmbiguousRef,
                            format!("casts play on {} TVs: {}", names.len(), names.join(", ")),
                        )
                        .with_hint("name the TV with --tv"));
                    }
                },
                Some(wanted) => {
                    let matching: Vec<&Entry> = live
                        .iter()
                        .copied()
                        .filter(|e| {
                            let cast = e.view.borrow();
                            names_tv(wanted, &cast.tv, &cast.tv_name)
                        })
                        .collect();
                    match matching.as_slice() {
                        [one] => *one,
                        [] => return Err(nothing_plays()),
                        _ => {
                            return Err(Error::new(
                                ErrorCode::AmbiguousRef,
                                format!("{wanted} names more than one TV"),
                            ));
                        }
                    }
                }
            };
            chosen.control.clone()
        };
        let (reply, answer) = oneshot::channel();
        control.send(Control::Act(action, reply)).map_err(|_| nothing_plays())?;
        answer.await.map_err(|_| nothing_plays())?
    }

    /// How the file a cast grant serves at `path` goes to the TV.
    pub fn served(&self, path: &str) -> Option<Dlna> {
        self.state().served.get(path).cloned()
    }

    /// The TV `wanted` names by its ID or a part of its name, or the only TV on the network.
    fn choose(&self, wanted: Option<&str>) -> Result<Arc<Renderer>, Error> {
        let state = self.state();
        let tvs: Vec<&Arc<Renderer>> = match wanted {
            Some(wanted) => {
                state.tvs.values().map(|k| &k.renderer).filter(|r| names_tv(wanted, &r.tv.id, &r.tv.name)).collect()
            }
            None => state.tvs.values().map(|k| &k.renderer).collect(),
        };
        match (tvs.as_slice(), wanted) {
            ([one], _) => Ok((*one).clone()),
            ([], None) => Err(Error::new(ErrorCode::NotFound, "no TV answered on the home network").with_hint(
                "check that the TV is on and on the same network as this Mac, and that its DLNA or media sharing setting is on",
            )),
            ([], Some(wanted)) => Err(Error::new(ErrorCode::NotFound, format!("no TV named {wanted} answered"))
                .with_hint("run torrential tv to list the TVs on the network")
                .with_next("torrential tv")),
            (many, _) => {
                let names: Vec<String> = many.iter().map(|r| at_address(&r.tv.name, &r.tv.id)).collect();
                Err(Error::new(ErrorCode::AmbiguousRef, format!("{} TVs answered: {}", names.len(), names.join(", ")))
                    .with_hint("name one with --tv, by its address if they share a name"))
            }
        }
    }

    /// Takes an ended cast off the list, unless another has taken its TV since.
    fn forget(&self, tv: &str, id: u64) {
        let mut state = self.state();
        if state.casts.get(tv).is_some_and(|entry| entry.id == id) {
            state.casts.remove(tv);
        }
    }
}

/// Waits until the TV plays the cast, and returns it then. A cast that fails first fails with why.
pub async fn playing(view: &mut watch::Receiver<Cast>) -> Result<Cast, Error> {
    let started = |cast: &Cast| matches!(cast.state, CastState::Playing | CastState::Paused | CastState::Ended { .. });
    let cast = view.wait_for(started).await.map_err(|_| Error::internal("the cast stopped without ending"))?.clone();
    match cast.state {
        CastState::Ended { error: Some(error) } => Err(error),
        _ => Ok(cast),
    }
}

fn nothing_plays() -> Error {
    Error::new(ErrorCode::NotFound, "no cast is playing").with_hint("start one with torrential tv play")
}

/// Whether `wanted` is the TV's ID or address, or a part of its name in any case.
fn names_tv(wanted: &str, id: &str, name: &str) -> bool {
    let wanted = wanted.trim();
    !wanted.is_empty()
        && (id == wanted || address(id) == wanted || name.to_lowercase().contains(&wanted.to_lowercase()))
}

/// The address in a TV's ID, which is its UDN and its address.
fn address(id: &str) -> &str {
    id.rsplit_once('@').map_or("", |(_, address)| address)
}

/// A TV's name with its address, which tells apart two TVs of the same name.
fn at_address(name: &str, id: &str) -> String {
    format!("{name} at {}", address(id))
}

/// The renderer that answered a search, with what its ConnectionManager says it plays. `None` for
/// a device that is not one, or does not answer.
async fn renderer(http: &reqwest::Client, heard: &Heard) -> Option<Renderer> {
    let answer = http.get(heard.location.clone()).timeout(DESCRIBE_WITHIN).send().await.ok()?;
    if !answer.status().is_success() {
        return None;
    }
    let description = upnp::describe(&upnp::read(answer).await.ok()??, &heard.location, heard.from)?;
    let sink = match &description.connection {
        Some(connection) => upnp::call(http, connection, "GetProtocolInfo", &[], &description.name)
            .await
            .ok()
            .and_then(|answer| xml::text(&answer, "Sink"))
            .map(|sink| upnp::sink(&sink))
            .unwrap_or_default(),
        None => Vec::new(),
    };
    Some(Renderer::new(description, heard.from.to_string(), sink))
}

/// What a cast found to play.
struct Found {
    id: TorrentId,
    file: u32,
    /// Relative to the torrent's folder, sanitized.
    path: String,
    size: u64,
    name: String,
    video: Option<VideoRef>,
    release: Option<ReleaseId>,
    /// Seconds.
    start_at: Option<f64>,
    subtitle: Option<(u32, String)>,
}

/// What a cast's task holds.
struct Run {
    ctx: Arc<Ctx>,
    tv: Arc<Renderer>,
    view: watch::Sender<Cast>,
    id: u64,
    /// The cast this one took the TV from, which may still be sending it a video.
    before: Option<watch::Receiver<Cast>>,
}

impl Run {
    async fn cast(
        self,
        request: CastRequest,
        mut control: mpsc::UnboundedReceiver<Control>,
        steps: broadcast::Sender<AttemptEvent>,
    ) {
        let found = tokio::select! {
            found = find(&self.ctx, &self.tv, &request, &steps, &self.view) => Some(found),
            () = until_stopped(&mut control) => None,
        };
        drop(steps);
        let error = match found {
            None => None,
            Some(Err(error)) => Some(error),
            Some(Ok(found)) => {
                self.view.send_modify(|cast| {
                    cast.name = found.name.clone();
                    cast.video = found.video.clone();
                    cast.download = Some(found.id.to_hex());
                    cast.file = Some(found.file);
                    cast.subtitle = found.subtitle.as_ref().map(|(_, path)| path.clone());
                    cast.state = CastState::Loading;
                });
                self.play(&found, &mut control).await.err()
            }
        };
        if let Some(error) = &error {
            super::log(&format!("the cast to {} ended: {error}", self.tv.tv.name));
        }
        // A cast that takes the TV from this one waits for this end, so it must mean the cast
        // before has ended too.
        if let Some(before) = self.before.clone() {
            ended(before).await;
        }
        self.view.send_modify(|cast| cast.state = CastState::Ended { error });
        drop(control);
        tokio::time::sleep(ENDED_KEPT).await;
        self.ctx.casts.forget(&self.tv.tv.id, self.id);
    }

    /// Sends the file to the TV and follows it until the cast ends.
    async fn play(&self, found: &Found, control: &mut mpsc::UnboundedReceiver<Control>) -> Result<(), Error> {
        if let Some(before) = self.before.clone() {
            tokio::select! {
                () = ended(before) => {}
                () = until_stopped(control) => return Ok(()),
            }
        }
        let ctx = &self.ctx;
        let tv = &self.tv.tv;
        let host = facing(&tv.address)?;
        let video = ctx.devices.grant(found.id, found.file, &found.path)?;
        let caption =
            found.subtitle.as_ref().map(|(index, path)| ctx.devices.grant(found.id, *index, path)).transpose()?;
        reachable(ctx, host).await?;
        let at = |path: &str| format!("http://{}{path}", std::net::SocketAddr::new(host, ctx.http_port));
        let url = at(video.path());
        let caption_url = caption.as_ref().map(|grant| at(grant.path()));
        let container = Container::of_path(&found.path).ok_or_else(|| refused(tv, &found.path))?;
        let mime = self.tv.mime(container).to_string();
        let mut served =
            vec![(video.path().to_string(), Dlna { mime: mime.clone(), caption: caption_url.clone(), video: true })];
        if let (Some(grant), Some((_, path))) = (&caption, &found.subtitle) {
            let kind = extension(path);
            served.push((grant.path().to_string(), Dlna { mime: format!("text/{kind}"), caption: None, video: false }));
        }
        let _serving = Serving::new(&ctx.casts, served);
        let duration = web_watch::probed(ctx, found.id, found.file).and_then(|media| media.duration);
        let film = found.video.as_ref().is_some_and(|video| video.episode.is_none());
        let kind = found.subtitle.as_ref().map(|(_, path)| extension(path));
        let metadata = didl::didl(&didl::Item {
            title: &found.name,
            url: &url,
            mime: &mime,
            size: found.size,
            duration,
            film,
            caption: caption_url.as_deref().zip(kind.as_deref()).map(|(url, kind)| didl::Caption { url, kind }),
        });
        self.send(&url, &metadata).await?;
        let token = video.path().trim_start_matches(super::devices::CAST).split('/').next().unwrap_or_default();
        self.follow(found, token, duration, control).await
    }

    /// Tells the TV to play `url`. A TV that refuses a new video while it plays one gets a Stop
    /// first. One that does not answer may still hold what it played before, which a Play would
    /// bring back, so the cast fails there.
    async fn send(&self, url: &str, metadata: &str) -> Result<(), Error> {
        let set = [("InstanceID", "0"), ("CurrentURI", url), ("CurrentURIMetaData", metadata)];
        match self.transport("SetAVTransportURI", &set).await {
            Ok(_) => {}
            Err(Failed::Refused(..)) => {
                let _ = self.transport("Stop", &[("InstanceID", "0")]).await;
                self.transport("SetAVTransportURI", &set).await?;
            }
            Err(gone @ Failed::Gone(_)) => return Err(gone.into()),
        }
        self.transport("Play", &[("InstanceID", "0"), ("Speed", "1")]).await?;
        Ok(())
    }

    /// Asks the TV where it is every second, keeps the position in the watch history, and carries
    /// out what the remote control asks, until the cast ends. A TV whose track no longer holds the
    /// grant's `token` plays something else now.
    async fn follow(
        &self,
        found: &Found,
        token: &str,
        known: Option<f64>,
        control: &mut mpsc::UnboundedReceiver<Control>,
    ) -> Result<(), Error> {
        let tv = &self.tv.tv;
        let started = Instant::now();
        let mut answered = Instant::now();
        let mut saved = Instant::now();
        let mut played = false;
        let mut resume = found.start_at.filter(|at| *at > WORTH_SAVING).map(|at| (at, RESUME_TRIES));
        let mut polls = 0u32;
        let mut tick = tokio::time::interval(POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                message = control.recv() => {
                    let Some(Control::Act(action, reply)) = message else {
                        self.save(found);
                        return Ok(());
                    };
                    let done = self.act(action).await;
                    let stop = action == CastAction::Stop;
                    if stop {
                        self.save(found);
                        self.view.send_modify(|cast| cast.state = CastState::Ended { error: None });
                    } else if matches!(action, CastAction::Pause) && done.is_ok() {
                        self.save(found);
                    }
                    let _ = reply.send(done.map(|()| self.view.borrow().clone()));
                    if stop {
                        return Ok(());
                    }
                    continue;
                }
            }
            polls += 1;
            match self.poll(polls % VOLUME_EVERY == 1).await {
                Ok(polled) => {
                    answered = Instant::now();
                    let elsewhere = polled.uri.as_deref().is_some_and(|uri| !uri.is_empty() && !uri.contains(token));
                    match polled.transport {
                        Transport::Playing | Transport::Paused if !elsewhere => {
                            played = true;
                            let paused = polled.transport == Transport::Paused;
                            let duration = polled.duration.or(known);
                            self.view.send_modify(|cast| {
                                cast.state = if paused { CastState::Paused } else { CastState::Playing };
                                cast.position = polled.position.or(cast.position);
                                cast.duration = duration;
                                cast.volume = polled.volume.or(cast.volume);
                            });
                            if !paused
                                && let Some((at, tries)) = resume.take()
                                && self.seek(at).await.is_err()
                                && tries > 1
                            {
                                resume = Some((at, tries - 1));
                            }
                        }
                        _ if played && elsewhere => {
                            self.save(found);
                            return Ok(());
                        }
                        Transport::Stopped if played => {
                            self.save(found);
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                Err(Failed::Refused(..)) => answered = Instant::now(),
                Err(Failed::Gone(_)) if answered.elapsed() >= LOST_AFTER => {
                    self.save(found);
                    return Err(Error::new(ErrorCode::NetworkUnavailable, format!("{} stopped answering", tv.name))
                        .with_hint("check that the TV is on, then press Play on TV again"));
                }
                Err(Failed::Gone(_)) => {}
            }
            if !played && started.elapsed() >= START_WITHIN {
                return Err(Error::new(
                    ErrorCode::FetchFailed,
                    format!("{} took the video but did not start playing it", tv.name),
                )
                .with_hint("check what the TV shows, and play it on this Mac if the TV says it cannot play the file"));
            }
            if saved.elapsed() >= SAVE_EVERY {
                self.save(found);
                saved = Instant::now();
            }
        }
    }

    async fn act(&self, action: CastAction) -> Result<(), Error> {
        let tv = &self.tv.tv;
        match action {
            CastAction::Pause => {
                self.transport("Pause", &[("InstanceID", "0")]).await?;
                self.view.send_modify(|cast| cast.state = CastState::Paused);
            }
            CastAction::Resume => {
                self.transport("Play", &[("InstanceID", "0"), ("Speed", "1")]).await?;
                self.view.send_modify(|cast| cast.state = CastState::Playing);
            }
            CastAction::Seek { position } => {
                let duration = self.view.borrow().duration;
                if !position.is_finite() || position < 0.0 || duration.is_some_and(|d| position > d) {
                    return Err(Error::new(ErrorCode::Usage, format!("{position} is not a time in this video")));
                }
                self.seek(position).await?;
                self.view.send_modify(|cast| cast.position = Some(position));
            }
            CastAction::Stop => {
                // A TV that has gone has stopped too.
                if let Err(Failed::Refused(_, error)) = self.transport("Stop", &[("InstanceID", "0")]).await {
                    super::log(&format!("{} refused to stop: {error}", tv.name));
                }
            }
            CastAction::Volume { level } => {
                let Some(rendering) = &self.tv.rendering else {
                    return Err(Error::new(
                        ErrorCode::Usage,
                        format!("torrential cannot set the volume of {}", tv.name),
                    ));
                };
                let level = level.min(100);
                let desired = level.to_string();
                let args = [("InstanceID", "0"), ("Channel", "Master"), ("DesiredVolume", desired.as_str())];
                self.call(rendering, "SetVolume", &args).await?;
                self.view.send_modify(|cast| cast.volume = Some(level));
            }
        }
        Ok(())
    }

    /// Seeks by the time into the video, or by the absolute time on a TV that has only that.
    async fn seek(&self, position: f64) -> Result<(), Failed> {
        let target = upnp::clock(position);
        let seek = |unit| [("InstanceID", "0"), ("Unit", unit), ("Target", target.as_str())];
        match self.transport("Seek", &seek("REL_TIME")).await {
            Err(Failed::Refused(710, _)) => self.transport("Seek", &seek("ABS_TIME")).await.map(drop),
            done => done.map(drop),
        }
    }

    async fn poll(&self, volume: bool) -> Result<Polled, Failed> {
        let transport = self.transport("GetTransportInfo", &[("InstanceID", "0")]).await?;
        let position = self.transport("GetPositionInfo", &[("InstanceID", "0")]).await?;
        let volume = match (&self.tv.rendering, volume) {
            (Some(rendering), true) => {
                let args = [("InstanceID", "0"), ("Channel", "Master")];
                let answer = self.call(rendering, "GetVolume", &args).await.ok();
                answer.and_then(|answer| xml::text(&answer, "CurrentVolume")?.parse::<u8>().ok()).map(|v| v.min(100))
            }
            _ => None,
        };
        Ok(Polled {
            transport: Transport::of(&xml::text(&transport, "CurrentTransportState").unwrap_or_default()),
            position: xml::text(&position, "RelTime").and_then(|t| upnp::seconds(&t)),
            duration: xml::text(&position, "TrackDuration").and_then(|t| upnp::seconds(&t)).filter(|d| *d > 0.0),
            uri: xml::text(&position, "TrackURI"),
            volume,
        })
    }

    async fn transport(&self, action: &str, args: &[(&str, &str)]) -> Result<String, Failed> {
        self.call(&self.tv.transport, action, args).await
    }

    async fn call(&self, service: &Service, action: &str, args: &[(&str, &str)]) -> Result<String, Failed> {
        upnp::call(&self.ctx.casts.http, service, action, args, &self.tv.tv.name).await
    }

    /// Keeps where the TV is in the watch history, as the web player keeps it.
    fn save(&self, found: &Found) {
        let cast = self.view.borrow().clone();
        let (Some(video), Some(position), Some(duration)) = (&found.video, cast.position, cast.duration) else {
            return;
        };
        if position <= WORTH_SAVING {
            return;
        }
        let watched = Watched {
            video: video.clone(),
            position: position.min(duration),
            duration: Some(duration),
            release: found.release,
            at: now(),
        };
        if let Err(error) = self.ctx.titles.shelf.watch(&watched) {
            super::log(&format!("cannot keep where {} stopped: {error}", found.name));
        }
    }
}

/// What a TV's AVTransport says it is doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Transport {
    Playing,
    Paused,
    /// Stopped, or holding nothing to play.
    Stopped,
    /// Loading, seeking, or a state torrential has no use for.
    Busy,
}

impl Transport {
    fn of(state: &str) -> Self {
        match state {
            "PLAYING" => Self::Playing,
            "PAUSED_PLAYBACK" => Self::Paused,
            "STOPPED" | "NO_MEDIA_PRESENT" => Self::Stopped,
            _ => Self::Busy,
        }
    }
}

/// What the TV said on one poll.
struct Polled {
    transport: Transport,
    position: Option<f64>,
    duration: Option<f64>,
    uri: Option<String>,
    volume: Option<u8>,
}

/// Returns once the cast `view` follows has ended, and can send its TV nothing more.
async fn ended(mut view: watch::Receiver<Cast>) {
    let _ = view.wait_for(Cast::ended).await;
}

/// Answers every action but a stop while a cast finds its source, and returns once it is stopped.
async fn until_stopped(control: &mut mpsc::UnboundedReceiver<Control>) {
    while let Some(message) = control.recv().await {
        match message {
            Control::Act(CastAction::Stop, reply) => {
                let _ = reply.send(Err(Error::new(ErrorCode::Interrupted, "the cast stopped before it started")));
                return;
            }
            Control::Act(_, reply) => {
                let _ = reply.send(Err(Error::new(ErrorCode::Usage, "the cast has not reached the TV yet")
                    .with_hint("wait until it plays, or stop it")));
            }
            Control::Leave => return,
        }
    }
    std::future::pending().await
}

/// The download and file to play: the first source of a title that starts, or a torrent's video.
async fn find(
    ctx: &Ctx,
    tv: &Renderer,
    request: &CastRequest,
    steps: &broadcast::Sender<AttemptEvent>,
    view: &watch::Sender<Cast>,
) -> Result<Found, Error> {
    let (id, file, name, video, release, resume_at) = match &request.target {
        Target::Title(video) => {
            let pin = match &request.source {
                Some(prefix) => Some(server::pinned(ctx, video, prefix).await?),
                None => None,
            };
            let ask = PlayTitle {
                video: video.clone(),
                pin,
                replace: None,
                playback: Some(tv.tv.decodes),
                containers: Containers::Tv(tv.tv.plays),
                finish: Finish::Stream,
            };
            let (mut card, mut end) = (None, None);
            attempt::run(ctx, ask, |step| {
                match &step {
                    AttemptEvent::Resolved { title, video, .. } => {
                        view.send_modify(|cast| cast.name = title_name(title, video.episode));
                        card = Some(title.clone());
                    }
                    AttemptEvent::Playing { .. } | AttemptEvent::Failed { .. } => end = Some(step.clone()),
                    _ => {}
                }
                let _ = steps.send(step);
            })
            .await;
            match end {
                Some(AttemptEvent::Playing { video, release, job, file, resume_at }) => {
                    let card =
                        card.ok_or_else(|| Error::internal("the title play played before it found the title"))?;
                    let name = title_name(&card, video.episode);
                    (job, file, name, Some(video), Some(release.id), resume_at)
                }
                Some(AttemptEvent::Failed { error, .. }) => return Err(error),
                _ => return Err(Error::internal("the title play ended without playing or failing")),
            }
        }
        target => {
            let deadline = std::time::Instant::now() + METADATA_WAIT;
            let (source, metainfo, playable) = play::prepare(ctx, target.clone(), request.file, deadline).await?;
            let path = sanitize(&metainfo.files[playable.video as usize].path);
            if !tv.tv.plays.takes(&path) {
                return Err(refused(&tv.tv, &path));
            }
            let (job, _) = play::begin(ctx, source, &playable, deadline).await?;
            let video = request.video.clone().or_else(|| job.title.as_ref().and_then(video_of));
            let release = job.title.as_ref().and_then(|link| link.release);
            let resume_at = match &video {
                Some(video) => resume_at(ctx, video)?,
                None => None,
            };
            let name = match &video {
                Some(video) => ctx
                    .titles
                    .catalog
                    .title(&video.title)
                    .await
                    .map_or_else(|_| file_name(&path), |details| title_name(&details.card, video.episode)),
                None => file_name(&path),
            };
            (job.id, playable.video, name, video, release, resume_at)
        }
    };
    let info = server::metadata(ctx, MetaTarget::Job(id), std::time::Instant::now() + METADATA_WAIT).await?;
    let entry = info
        .metainfo
        .files
        .get(file as usize)
        .ok_or_else(|| Error::internal(format!("the download has no file {file}")))?;
    let path = sanitize(&entry.path);
    let subtitle = match request.subtitle {
        None => None,
        Some(index) => {
            let playable = play::choose(&info.metainfo, Some(file))?;
            if !playable.subtitles.contains(&index) {
                return Err(Error::new(
                    ErrorCode::Usage,
                    format!("file {index} is not a subtitle of {}", file_name(&path)),
                )
                .with_hint(format!("run torrential files {} to list its files", &id.to_hex()[..8])));
            }
            Some((index, sanitize(&info.metainfo.files[index as usize].path)))
        }
    };
    Ok(Found {
        id,
        file,
        size: entry.size,
        path,
        name,
        video,
        release,
        start_at: request.start_at.or(resume_at),
        subtitle,
    })
}

/// The film or episode a download made by playing a title holds, if it holds just one.
fn video_of(link: &TitleLink) -> Option<VideoRef> {
    let episode = match link.span {
        Span::Film => None,
        Span::Episode(episode) => Some(episode),
        Span::Season(_) | Span::Seasons { .. } | Span::Complete => return None,
    };
    Some(VideoRef { title: link.title.clone(), episode })
}

/// Where the person stopped `video`, unless they finished it.
fn resume_at(ctx: &Ctx, video: &VideoRef) -> Result<Option<f64>, Error> {
    let watched = ctx.titles.shelf.watched(&video.title)?;
    Ok(watched.iter().find(|w| &w.video == video).filter(|w| !w.finished()).map(|w| w.position))
}

fn refused(tv: &Tv, path: &str) -> Error {
    let container = extension(path).to_ascii_uppercase();
    let plays: Vec<String> = tv.plays.iter().map(|c| c.name().to_ascii_uppercase()).collect();
    Error::new(ErrorCode::Usage, format!("{} does not play {container} files, only {}", tv.name, plays.join(" and ")))
        .with_hint("play it on this Mac, or choose a source in a format the TV plays")
}

fn extension(path: &str) -> String {
    path.rsplit_once('.').map_or("", |(_, extension)| extension).to_ascii_lowercase()
}

fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// This Mac's address on the network the TV at `address` is on.
fn facing(address: &str) -> Result<IpAddr, Error> {
    let unreachable = |e: std::io::Error| {
        Error::new(ErrorCode::NetworkUnavailable, format!("this Mac has no way to the TV at {address} ({e})"))
    };
    let tv: IpAddr = address.parse().map_err(|_| Error::internal(format!("{address} is not an IP address")))?;
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(unreachable)?;
    socket.connect((tv, ssdp::GROUP.port())).map_err(unreachable)?;
    Ok(socket.local_addr().map_err(unreachable)?.ip())
}

/// Waits until the engine takes connections at `host`, as the TV will make them. The engine
/// listens on the home network once a grant is live.
async fn reachable(ctx: &Ctx, host: IpAddr) -> Result<(), Error> {
    let deadline = Instant::now() + REACHABLE_WITHIN;
    while tokio::net::TcpStream::connect((host, ctx.http_port)).await.is_err() {
        if Instant::now() >= deadline {
            let why = ctx.devices.list(ctx.http_port).unreachable;
            return Err(Error::new(
                ErrorCode::NetworkUnavailable,
                why.unwrap_or_else(|| format!("the TV cannot reach this Mac at {host}:{}", ctx.http_port)),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

/// The files a cast serves, listed for the HTTP server until it is dropped.
struct Serving<'a> {
    casts: &'a Casts,
    paths: Vec<String>,
}

impl<'a> Serving<'a> {
    fn new(casts: &'a Casts, served: Vec<(String, Dlna)>) -> Self {
        let paths = served.iter().map(|(path, _)| path.clone()).collect();
        casts.state().served.extend(served);
        Self { casts, paths }
    }
}

impl Drop for Serving<'_> {
    fn drop(&mut self) {
        let mut state = self.casts.state();
        for path in &self.paths {
            state.served.remove(path);
        }
    }
}

#[cfg(test)]
mod tests;
