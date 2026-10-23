use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, mpsc, oneshot};
use torrential_core::protocol::{Change, GetOutcome, GetReply};
use torrential_core::{Error, ErrorCode, HexPrefix, InfoHash, Job, PieceMap, Release, Selection, Target, TorrentId};

use super::admission::{self, Cap, Disk};
use super::book::{self, Intent, JobEntry, JobRecord, Observed, Phase, Verify};
use super::engine_port::{
    Activity, AddSource, AddTorrent, EngineCommand, EngineEvent, EngineHandle, Metainfo, ParseTorrent,
};
use super::files;
use super::magnet::{self, Magnet};
use super::resolve::{self, Found, Scope};
use super::search_port::SavedLink;
use super::store::{Store, Tombstone};

#[derive(Clone, Debug)]
pub struct OwnerConfig {
    pub default_dir: PathBuf,
    pub probe_dir: PathBuf,
    pub max_active: usize,
    pub idle_timeout: Option<Duration>,
    pub resume_on_start: bool,
    pub save_resume_every: Duration,
    pub stall_after: Duration,
    pub writes_stop_within: Duration,
}

pub type Reply<T> = oneshot::Sender<Result<T, Error>>;

#[derive(Clone, Debug)]
pub enum Source {
    Download(TorrentId),
    Magnet(Magnet),
    Torrent { metainfo: Metainfo, origin: Origin },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    File,
    Url,
    Cache,
    Peers,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "torrent_file",
            Self::Url => "torrent_url",
            Self::Cache => "cache",
            Self::Peers => "peers",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Resolved {
    Job(TorrentId),
    Release { release: Box<Release>, cached: Option<Arc<Metainfo>> },
}

#[derive(Clone, Debug)]
pub struct FilesInfo {
    pub id: TorrentId,
    pub metainfo: Arc<Metainfo>,
    pub source: &'static str,
    pub download: Option<DownloadFiles>,
}

#[derive(Clone, Debug)]
pub struct DownloadFiles {
    pub wanted: Vec<bool>,
    pub verified: FileBytes,
}

#[derive(Clone, Debug)]
pub enum FileBytes {
    Known(Vec<u64>),
    Engine { saved: Vec<u64> },
}

#[derive(Clone, Debug)]
pub enum MetaTarget {
    Job(TorrentId),
    Magnet(Magnet),
}

pub struct GetCommand {
    pub source: Source,
    pub dir: Option<PathBuf>,
    pub files: Vec<String>,
    pub max_size: Option<u64>,
    pub setup_max_size: Option<u64>,
    pub dry_run: bool,
}

pub enum Command {
    Resolve { prefix: HexPrefix, results: Vec<Release>, command: &'static str, reply: Reply<Resolved> },
    Get { get: Box<GetCommand>, reply: Reply<GetReply> },
    Metadata { target: MetaTarget, deadline: Instant, reply: Reply<FilesInfo> },
    Cache { metainfo: Metainfo },
    Status { refs: Vec<String>, reply: Reply<Vec<Job>> },
    Pause { refs: Vec<String>, reply: Reply<Vec<Change>> },
    Resume { refs: Vec<String>, all: bool, reply: Reply<Vec<Change>> },
    Remove { refs: Vec<String>, reply: Reply<Vec<Change>> },
    Verify { reference: String, repair: bool, reply: Reply<Vec<Change>> },
    Watch { refs: Vec<String>, reply: Reply<(Vec<Job>, broadcast::Receiver<Update>)> },
    RecordResults(Vec<(Release, Option<SavedLink>)>),
    Connected,
    Disconnected { keep_until: Option<Instant> },
    Shutdown { reply: oneshot::Sender<()> },
}

#[derive(Clone, Debug)]
pub enum Update {
    Job(Box<Job>),
    Removed(TorrentId),
}

struct MetadataWaiter {
    id: TorrentId,
    deadline: Instant,
    reply: Reply<FilesInfo>,
}

struct PendingPause {
    waiting: BTreeSet<TorrentId>,
    changes: Vec<Change>,
    deadline: Instant,
    reply: Reply<Vec<Change>>,
}

struct Stopping {
    waiting: BTreeSet<TorrentId>,
    deadline: Instant,
    replies: Vec<oneshot::Sender<()>>,
}

pub struct Owner {
    store: Store,
    jobs: BTreeMap<TorrentId, JobEntry>,
    tombstones: Vec<Tombstone>,
    probes: BTreeSet<TorrentId>,
    draining: BTreeSet<TorrentId>,
    metadata_waiters: Vec<MetadataWaiter>,
    pauses: Vec<PendingPause>,
    stopping: Option<Stopping>,
    stopped: bool,
    engine: EngineHandle,
    parse: ParseTorrent,
    disk: Box<dyn Disk>,
    updates: broadcast::Sender<Update>,
    config: OwnerConfig,
    connections: usize,
    idle_since: Instant,
    keep_until: Option<Instant>,
    resume_saved_at: Instant,
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Owner {
    pub fn new(
        store: Store,
        engine: EngineHandle,
        parse: ParseTorrent,
        disk: Box<dyn Disk>,
        config: OwnerConfig,
    ) -> Result<Self, Error> {
        let mut jobs = BTreeMap::new();
        for stored in store.jobs()? {
            let metainfo = match &stored.metainfo {
                Some(bytes) => Some(Arc::new(parse(bytes).map_err(|e| {
                    Error::new(
                        ErrorCode::StateCorrupt,
                        format!("the stored file list of {} does not parse: {}", stored.record.id, e.message),
                    )
                })?)),
                None => None,
            };
            let observed = match stored.record.intent {
                Intent::Running if !config.resume_on_start => Observed::Parked,
                _ => Observed::Absent,
            };
            jobs.insert(stored.record.id, JobEntry::new(stored.record, metainfo, observed));
        }
        let now = now();
        let (updates, _) = broadcast::channel(256);
        let mut owner = Self {
            tombstones: store.tombstones(now)?,
            store,
            jobs,
            probes: BTreeSet::new(),
            draining: BTreeSet::new(),
            metadata_waiters: Vec::new(),
            pauses: Vec::new(),
            stopping: None,
            stopped: false,
            engine,
            parse,
            disk,
            updates,
            config,
            connections: 0,
            idle_since: Instant::now(),
            keep_until: None,
            resume_saved_at: Instant::now(),
        };
        owner.schedule();
        Ok(owner)
    }

    pub async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut events: mpsc::UnboundedReceiver<EngineEvent>,
    ) {
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        while !self.stopped {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.command(command),
                    None => self.begin_stop(None),
                },
                event = events.recv() => match event {
                    Some(event) => self.engine_event(event),
                    None => {
                        super::log("the engine thread stopped, so the engine process is exiting");
                        self.stopped = true;
                    }
                },
                _ = tick.tick() => self.tick(),
            }
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Resolve { prefix, results, command, reply } => {
                let _ = reply.send(self.resolve_any(&prefix, &results, command));
            }
            Command::Get { get, reply } => {
                let _ = reply.send(self.get(*get));
            }
            Command::Metadata { target, deadline, reply } => self.metadata(target, deadline, reply),
            Command::Cache { metainfo } => {
                let _ = self.store.cache_metainfo(metainfo.info_hash.id(), &metainfo.bytes, now());
            }
            Command::Status { refs, reply } => {
                let _ = reply.send(self.resolve_jobs(&refs, "status").map(|ids| self.views(&ids)));
            }
            Command::Pause { refs, reply } => self.pause(refs, reply),
            Command::Resume { refs, all, reply } => {
                let _ = reply.send(self.resume(&refs, all));
            }
            Command::Remove { refs, reply } => {
                let _ = reply.send(self.remove(&refs));
            }
            Command::Verify { reference, repair, reply } => {
                let _ = reply.send(self.verify(&reference, repair));
            }
            Command::Watch { refs, reply } => {
                let watch = self.resolve_jobs(&refs, "wait").map(|ids| (self.views(&ids), self.updates.subscribe()));
                let _ = reply.send(watch);
            }
            Command::RecordResults(releases) => self.record_results(releases),
            Command::Connected => self.connections += 1,
            Command::Disconnected { keep_until } => {
                self.connections = self.connections.saturating_sub(1);
                self.keep_until = self.keep_until.max(keep_until);
            }
            Command::Shutdown { reply } => self.begin_stop(Some(reply)),
        }
    }

    fn views(&self, ids: &[TorrentId]) -> Vec<Job> {
        ids.iter().filter_map(|id| self.jobs.get(id)).map(JobEntry::view).collect()
    }

    fn publish(&self, id: TorrentId) {
        if let Some(job) = self.jobs.get(&id) {
            let _ = self.updates.send(Update::Job(Box::new(job.view())));
        }
    }

    fn resolve_jobs(&self, refs: &[String], command: &'static str) -> Result<Vec<TorrentId>, Error> {
        if refs.is_empty() {
            let mut all: Vec<&JobEntry> = self.jobs.values().collect();
            all.sort_by_key(|j| (j.record.added_at, j.id()));
            return Ok(all.into_iter().map(JobEntry::id).collect());
        }
        let mut ids = Vec::new();
        for reference in refs {
            let id = self.resolve_job(reference, command)?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    fn resolve_job(&self, reference: &str, command: &'static str) -> Result<TorrentId, Error> {
        match Target::parse(reference, Path::new("/"))? {
            Target::Prefix(prefix) => {
                let prefix: HexPrefix = prefix.parse()?;
                let jobs: Vec<(TorrentId, Option<&str>)> =
                    self.jobs.values().map(|j| (j.id(), j.record.name.as_deref())).collect();
                match resolve::prefix(&prefix, &jobs, &[], Scope::Downloads, command)? {
                    Found::Job(id) => Ok(id),
                    Found::Release(_) => unreachable!("downloads scope finds only downloads"),
                }
            }
            Target::InfoHash(hash) => self.find_job(&hash).ok_or_else(|| no_download(reference)),
            Target::Magnet(uri) => {
                let hash = magnet::parse(&uri)?.info_hash;
                self.find_job(&hash).ok_or_else(|| no_download(&hash.id().to_hex()))
            }
            Target::TorrentUrl(_) | Target::TorrentFile(_) => Err(Error::new(
                ErrorCode::InvalidRef,
                format!("{command} takes a download ID, not a .torrent file or URL"),
            )
            .with_hint("run torrential status to list download IDs")
            .with_next("torrential status")),
        }
    }

    fn find_job(&self, hash: &InfoHash) -> Option<TorrentId> {
        if self.jobs.contains_key(&hash.id()) {
            return Some(hash.id());
        }
        self.store.alias_owners(hash).ok()?.into_iter().find(|id| self.jobs.contains_key(id))
    }

    fn resolve_any(&self, prefix: &HexPrefix, results: &[Release], command: &'static str) -> Result<Resolved, Error> {
        let jobs: Vec<(TorrentId, Option<&str>)> =
            self.jobs.values().map(|j| (j.id(), j.record.name.as_deref())).collect();
        match resolve::prefix(prefix, &jobs, results, Scope::DownloadsAndResults, command)? {
            Found::Job(id) => Ok(Resolved::Job(id)),
            Found::Release(release) => {
                let cached = match release.id {
                    torrential_core::ReleaseId::Torrent(id) => self.cached(id),
                    torrential_core::ReleaseId::Locator(_) => None,
                };
                Ok(Resolved::Release { release: Box::new(release.clone()), cached })
            }
        }
    }

    fn cached(&self, id: TorrentId) -> Option<Arc<Metainfo>> {
        let bytes = self.store.metainfo(id).ok().flatten()?;
        (self.parse)(&bytes).ok().map(Arc::new)
    }

    fn record_results(&mut self, saved: Vec<(Release, Option<SavedLink>)>) {
        let _ = self.store.record_results(&saved, now());
    }

    fn others_remaining(&self, dir: &Path, except: TorrentId) -> u64 {
        let Ok(space) = self.disk.space(dir) else { return 0 };
        self.jobs
            .values()
            .filter(|j| j.id() != except && j.record.intent == Intent::Running)
            .filter(|j| self.disk.space(&j.record.dir).is_ok_and(|s| s.device == space.device))
            .map(JobEntry::remaining)
            .sum()
    }

    fn get(&mut self, get: GetCommand) -> Result<GetReply, Error> {
        let GetCommand { source, dir, files, max_size, setup_max_size, dry_run } = get;
        if !files.is_empty() {
            super::files::compile(&files)?;
        }
        let selection = if files.is_empty() { Selection::All } else { Selection::Patterns(files) };
        let info_hash = match &source {
            Source::Download(id) if self.jobs.contains_key(id) => {
                return self.get_existing(*id, source, selection, max_size, dry_run);
            }
            Source::Download(id) => return Err(no_download(&id.to_hex())),
            Source::Magnet(m) => m.info_hash,
            Source::Torrent { metainfo, .. } => metainfo.info_hash,
        };
        if let Some(id) = self.find_job(&info_hash) {
            return self.get_existing(id, source, selection, max_size, dry_run);
        }
        let id = info_hash.id();
        let (metainfo, magnet) = match source {
            Source::Torrent { metainfo, .. } => (Some(Arc::new(metainfo)), None),
            Source::Magnet(m) => (self.cached(id), Some(m)),
            Source::Download(_) => unreachable!("handled above"),
        };
        let name = metainfo.as_ref().map(|m| m.name.clone()).or_else(|| magnet.as_ref().and_then(|m| m.name.clone()));
        let dir = dir.unwrap_or_else(|| self.config.default_dir.clone());
        let cap = Cap::new(max_size, setup_max_size);
        let plan =
            metainfo.as_deref().map(|m| admission::plan(m, &selection, &dir, None, self.disk.as_ref())).transpose()?;
        if let (Some(plan), Some(metainfo)) = (&plan, metainfo.as_deref()) {
            admission::check_size(plan.selected_size, cap, None)?;
            if !dry_run {
                admission::check_conflicts(plan)?;
                self.check_claims(id, metainfo, &dir, &plan.wanted, None)?;
            }
            admission::check_disk(plan.space, plan.selected_size, self.others_remaining(&dir, id))?;
        }
        let record = JobRecord {
            id,
            info_hash: metainfo.as_ref().map_or(info_hash, |m| m.info_hash),
            name,
            dir,
            selection,
            cap,
            magnet: magnet.map(|m| m.uri),
            added_at: now(),
            intent: Intent::Running,
            verified: 0,
            piece_map: None,
            files_verified: None,
        };
        if dry_run {
            let job = JobEntry::new(record, metainfo, Observed::Absent).view();
            return Ok(GetReply {
                outcome: GetOutcome::Planned,
                job,
                plan: plan.map(|p| p.public(cap)),
                previous_selection: None,
            });
        }
        self.store.insert(&record, metainfo.as_ref().map(|m| m.bytes.as_slice()), record.added_at)?;
        self.tombstones.retain(|t| t.id != id);
        if self.probes.remove(&id) {
            self.engine.send(EngineCommand::Remove(id));
            self.draining.insert(id);
        }
        self.jobs.insert(id, JobEntry::new(record, metainfo, Observed::Absent));
        self.schedule();
        self.publish(id);
        let job = &self.jobs[&id];
        let outcome = if job.observed.in_engine() { GetOutcome::Started } else { GetOutcome::Queued };
        Ok(GetReply { outcome, job: job.view(), plan: plan.map(|p| p.public(cap)), previous_selection: None })
    }

    fn get_existing(
        &mut self,
        id: TorrentId,
        source: Source,
        selection: Selection,
        max_size: Option<u64>,
        dry_run: bool,
    ) -> Result<GetReply, Error> {
        let cap = max_size.map_or(self.jobs[&id].record.cap, |bytes| Cap { bytes, from_setup: false });
        if !dry_run && self.jobs[&id].metainfo.is_none() && cap != self.jobs[&id].record.cap {
            self.store.set_cap(id, cap)?;
            self.jobs.get_mut(&id).expect("found above").record.cap = cap;
        }
        if let Source::Torrent { metainfo, .. } = source
            && self.jobs[&id].metainfo.is_none()
        {
            self.job_metadata(id, metainfo);
        }
        if !dry_run && self.jobs[&id].observed == Observed::Parked {
            self.jobs.get_mut(&id).expect("found above").observed = Observed::Absent;
            self.schedule();
            self.publish(id);
        }
        let job = &self.jobs[&id];
        let too_large = matches!(&job.record.intent, Intent::Failed { error } if error.code == ErrorCode::TooLarge);
        let unchanged = selection == Selection::All || selection == job.record.selection;
        if unchanged && cap == job.record.cap && !too_large {
            let plan = self.current_plan(job);
            return Ok(GetReply { outcome: GetOutcome::Existing, job: job.view(), plan, previous_selection: None });
        }
        let previous = (!unchanged).then(|| job.record.selection.clone());
        let selection = if unchanged { job.record.selection.clone() } else { selection };
        let Some(metainfo) = job.metainfo.clone() else {
            if dry_run {
                return Ok(GetReply {
                    outcome: GetOutcome::Planned,
                    job: job.view(),
                    plan: None,
                    previous_selection: previous,
                });
            }
            self.store.set_selection(id, &selection)?;
            self.jobs.get_mut(&id).expect("found above").record.selection = selection;
            self.publish(id);
            let job = self.jobs[&id].view();
            return Ok(GetReply { outcome: GetOutcome::Updated, job, plan: None, previous_selection: previous });
        };
        let before = if too_large { None } else { job.wanted() };
        let plan = admission::plan(&metainfo, &selection, &job.record.dir, before.as_deref(), self.disk.as_ref())?;
        admission::check_size(plan.selected_size, cap, None)?;
        if !dry_run {
            admission::check_conflicts(&plan)?;
            self.check_claims(id, &metainfo, &job.record.dir, &plan.wanted, None)?;
        }
        let needed = plan.selected_size.saturating_sub(job.verified());
        admission::check_disk(plan.space, needed, self.others_remaining(&job.record.dir, id))?;
        if dry_run {
            let mut preview = job.clone();
            preview.record.selection = selection;
            let job = preview.view();
            return Ok(GetReply {
                outcome: GetOutcome::Planned,
                job,
                plan: Some(plan.public(cap)),
                previous_selection: previous,
            });
        }
        if previous.is_some() {
            self.store.set_selection(id, &selection)?;
        }
        if cap != job.record.cap {
            self.store.set_cap(id, cap)?;
        }
        let reopen = too_large || (previous.is_some() && matches!(job.record.intent, Intent::Done { .. }));
        if reopen {
            self.store.set_intent(id, &Intent::Running)?;
        }
        let job = self.jobs.get_mut(&id).expect("found above");
        job.record.selection = selection;
        job.record.cap = cap;
        if reopen {
            job.record.intent = Intent::Running;
        }
        if previous.is_some() && job.observed.in_engine() {
            self.engine.send(EngineCommand::Select(id, plan.wanted.clone()));
        }
        self.schedule();
        self.publish(id);
        let job = &self.jobs[&id];
        let outcome = match (&previous, too_large) {
            (Some(_), _) => GetOutcome::Updated,
            (None, true) if job.observed.in_engine() => GetOutcome::Started,
            (None, true) => GetOutcome::Queued,
            (None, false) => GetOutcome::Existing,
        };
        Ok(GetReply { outcome, job: job.view(), plan: Some(plan.public(cap)), previous_selection: previous })
    }

    fn current_plan(&self, job: &JobEntry) -> Option<torrential_core::protocol::DownloadPlan> {
        let metainfo = job.metainfo.as_deref()?;
        let wanted = job.wanted()?;
        let plan = admission::plan(metainfo, &job.record.selection, &job.record.dir, Some(&wanted), self.disk.as_ref())
            .ok()?;
        Some(plan.public(job.record.cap))
    }

    fn metadata(&mut self, target: MetaTarget, deadline: Instant, reply: Reply<FilesInfo>) {
        let id = match target {
            MetaTarget::Job(id) => id,
            MetaTarget::Magnet(magnet) => match self.find_job(&magnet.info_hash) {
                Some(id) => id,
                None => {
                    let id = magnet.info_hash.id();
                    if let Some(metainfo) = self.cached(id) {
                        let _ =
                            reply.send(Ok(FilesInfo { id, metainfo, source: Origin::Cache.as_str(), download: None }));
                        return;
                    }
                    if !self.probes.contains(&id) && !self.draining.contains(&id) {
                        self.probes.insert(id);
                        self.engine.send(EngineCommand::Add(AddTorrent {
                            id,
                            source: AddSource::Magnet(magnet.uri),
                            save_path: self.config.probe_dir.clone(),
                            wanted: None,
                            resume_data: None,
                            paused: false,
                        }));
                    }
                    self.metadata_waiters.push(MetadataWaiter { id, deadline, reply });
                    return;
                }
            },
        };
        match self.job_files(id) {
            Some(info) => {
                let _ = reply.send(Ok(info));
            }
            None => self.metadata_waiters.push(MetadataWaiter { id, deadline, reply }),
        }
    }

    fn job_files(&self, id: TorrentId) -> Option<FilesInfo> {
        let job = self.jobs.get(&id)?;
        let metainfo = job.metainfo.clone()?;
        let wanted = job.wanted().unwrap_or_else(|| vec![false; metainfo.files.len()]);
        let verified = match (&job.record.intent, job.observed) {
            (Intent::Done { .. }, _) => {
                FileBytes::Known(metainfo.files.iter().zip(&wanted).map(|(f, w)| if *w { f.size } else { 0 }).collect())
            }
            (_, Observed::InEngine(_)) => {
                FileBytes::Engine { saved: job.record.files_verified.clone().unwrap_or_default() }
            }
            _ => FileBytes::Known(job.record.files_verified.clone().unwrap_or_default()),
        };
        let download = DownloadFiles { wanted, verified };
        Some(FilesInfo { id, metainfo, source: "download", download: Some(download) })
    }

    fn reply_metadata_waiters(&mut self, id: TorrentId, info: Result<FilesInfo, Error>) {
        let (ready, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.metadata_waiters).into_iter().partition(|w| w.id == id);
        self.metadata_waiters = rest;
        for waiter in ready {
            let _ = waiter.reply.send(info.clone());
        }
    }

    fn pause(&mut self, refs: Vec<String>, reply: Reply<Vec<Change>>) {
        let ids = match self.resolve_jobs(&refs, "pause") {
            Ok(ids) => ids,
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        };
        let mut changes = Vec::new();
        let mut waiting = BTreeSet::new();
        for id in ids {
            let job = &self.jobs[&id];
            let result = match &job.record.intent {
                Intent::Paused => "already_paused",
                Intent::Done { .. } => "already_done",
                Intent::Failed { .. } => "already_stopped",
                Intent::Running => {
                    if let Err(e) = self.store.set_intent(id, &Intent::Paused) {
                        let _ = reply.send(Err(e));
                        return;
                    }
                    let job = self.jobs.get_mut(&id).expect("resolved above");
                    job.record.intent = Intent::Paused;
                    match job.observed {
                        Observed::InEngine(Phase::Paused | Phase::Pausing) => {}
                        Observed::InEngine(_) => {
                            job.observed = Observed::InEngine(Phase::Pausing);
                            self.engine.send(EngineCommand::Pause(id));
                        }
                        Observed::Absent | Observed::Parked => job.observed = Observed::Absent,
                    }
                    self.publish(id);
                    "paused"
                }
            };
            if self.jobs[&id].observed == Observed::InEngine(Phase::Pausing) {
                waiting.insert(id);
            }
            changes.push(self.change(id, result));
        }
        self.schedule();
        if waiting.is_empty() {
            let _ = reply.send(Ok(changes));
        } else {
            let deadline = Instant::now() + self.config.writes_stop_within;
            self.pauses.push(PendingPause { waiting, changes, deadline, reply });
        }
    }

    fn change(&self, id: TorrentId, result: &str) -> Change {
        let job = &self.jobs[&id];
        Change { id: id.to_hex(), name: job.record.name.clone(), result: result.to_string(), path: None }
    }

    fn resume(&mut self, refs: &[String], all: bool) -> Result<Vec<Change>, Error> {
        let resumable = |j: &JobEntry| {
            matches!(j.record.intent, Intent::Paused | Intent::Failed { .. })
                || (j.record.intent == Intent::Running && j.observed == Observed::Parked)
        };
        let fits = |j: &JobEntry| {
            j.selected_size().map_or(Ok(()), |size| admission::check_size(size, j.record.cap, Some(j.id())))
        };
        let ids = if all {
            self.jobs.values().filter(|j| resumable(j) && fits(j).is_ok()).map(JobEntry::id).collect()
        } else {
            let ids = self.resolve_jobs(refs, "resume")?;
            for job in ids.iter().map(|id| &self.jobs[id]).filter(|j| resumable(j)) {
                fits(job)?;
            }
            ids
        };
        let mut changes = Vec::new();
        for id in ids {
            let job = &self.jobs[&id];
            let result = match (&job.record.intent, job.observed) {
                (Intent::Done { .. }, _) => "already_done",
                (Intent::Running, Observed::Parked) => {
                    self.jobs.get_mut(&id).expect("resolved above").observed = Observed::Absent;
                    "resumed"
                }
                (Intent::Running, _) => "already_running",
                (Intent::Paused | Intent::Failed { .. }, observed) => {
                    self.store.set_intent(id, &Intent::Running)?;
                    let job = self.jobs.get_mut(&id).expect("resolved above");
                    job.record.intent = Intent::Running;
                    if observed == Observed::Parked {
                        job.observed = Observed::Absent;
                    }
                    "resumed"
                }
            };
            changes.push(self.change(id, result));
        }
        self.schedule();
        for change in &changes {
            if let Ok(id) = change.id.parse() {
                self.publish(id);
            }
        }
        Ok(changes)
    }

    fn remove(&mut self, refs: &[String]) -> Result<Vec<Change>, Error> {
        let mut targets = Vec::new();
        for reference in refs {
            match self.resolve_job(reference, "rm") {
                Ok(id) => targets.push(Ok(id)),
                Err(e) if e.code == ErrorCode::NotFound => match self.tombstone(reference) {
                    Some(tomb) => targets.push(Err(tomb.clone())),
                    None => return Err(e),
                },
                Err(e) => return Err(e),
            }
        }
        let now = now();
        let mut changes = Vec::new();
        for target in targets {
            match target {
                Err(tomb) => changes.push(Change {
                    id: tomb.id.to_hex(),
                    name: tomb.name,
                    result: "already_removed".into(),
                    path: tomb.path,
                }),
                Ok(id) if !self.jobs.contains_key(&id) => {}
                Ok(id) => {
                    let job = &self.jobs[&id];
                    let view = job.view();
                    let tomb =
                        Tombstone { id, name: job.record.name.clone(), path: view.path.clone(), removed_at: now };
                    self.store.remove(&tomb)?;
                    let job = self.jobs.remove(&id).expect("resolved above");
                    if job.observed.in_engine() {
                        self.engine.send(EngineCommand::Remove(id));
                        self.draining.insert(id);
                    }
                    self.pauses.iter_mut().for_each(|p| {
                        p.waiting.remove(&id);
                    });
                    self.tombstones.retain(|t| t.id != id);
                    self.tombstones.push(tomb);
                    let _ = self.updates.send(Update::Removed(id));
                    changes.push(Change {
                        id: id.to_hex(),
                        name: job.record.name,
                        result: "removed".into(),
                        path: view.path,
                    });
                }
            }
        }
        self.finish_pauses();
        self.schedule();
        Ok(changes)
    }

    fn tombstone(&self, reference: &str) -> Option<&Tombstone> {
        match Target::parse(reference, Path::new("/")).ok()? {
            Target::Prefix(p) => {
                let prefix: HexPrefix = p.parse().ok()?;
                let mut matching = self.tombstones.iter().filter(|t| t.id.has_prefix(&prefix));
                let first = matching.next()?;
                matching.next().is_none().then_some(first)
            }
            Target::InfoHash(hash) => self.tombstones.iter().find(|t| t.id == hash.id()),
            _ => None,
        }
    }

    fn verify(&mut self, reference: &str, repair: bool) -> Result<Vec<Change>, Error> {
        let id = self.resolve_job(reference, "verify")?;
        let job = &self.jobs[&id];
        let short = &id.to_hex()[..6];
        let Some(metainfo) = job.metainfo.clone() else {
            return Err(Error::new(
                ErrorCode::JobConflict,
                format!("{short} has no file list yet, so there is nothing to verify"),
            )
            .with_hint(format!("torrential wait {short} --until metadata"))
            .with_next(format!("torrential wait {short} --until metadata")));
        };
        if repair && let Some(size) = job.selected_size() {
            admission::check_size(size, job.record.cap, Some(id))?;
        }
        if job.verify.is_some() || job.observed == Observed::InEngine(Phase::Checking) {
            return Ok(vec![self.change(id, "checking")]);
        }
        if matches!(job.observed, Observed::InEngine(Phase::Pausing | Phase::Finishing { .. })) {
            return Err(Error::new(ErrorCode::JobConflict, format!("{short} is busy saving its state"))
                .with_hint(format!("try torrential verify {short} again in a few seconds")));
        }
        self.check_writes(id, "verify")?;
        let job = &self.jobs[&id];
        let wanted = job.wanted().unwrap_or_else(|| vec![true; metainfo.files.len()]);
        let in_engine = job.observed.in_engine();
        let dir = job.record.dir.clone();
        if !in_engine {
            let resume_data = self.store.resume(id)?;
            self.engine.send(EngineCommand::Add(AddTorrent {
                id,
                source: AddSource::Metainfo(metainfo.bytes.clone()),
                save_path: dir,
                wanted: Some(wanted),
                resume_data,
                paused: true,
            }));
        }
        self.engine.send(EngineCommand::Recheck(id));
        let job = self.jobs.get_mut(&id).expect("resolved above");
        job.verify = Some(if repair { Verify::Repair } else { Verify::Report });
        job.observed = Observed::InEngine(Phase::Checking);
        self.publish(id);
        Ok(vec![self.change(id, "checking")])
    }

    fn engine_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::Metadata { id, metainfo } => {
                if self.probes.remove(&id) {
                    let _ = self.store.cache_metainfo(id, &metainfo.bytes, now());
                    self.engine.send(EngineCommand::Remove(id));
                    self.draining.insert(id);
                    let info =
                        FilesInfo { id, metainfo: Arc::new(metainfo), source: Origin::Peers.as_str(), download: None };
                    self.reply_metadata_waiters(id, Ok(info));
                } else if self.jobs.get(&id).is_some_and(|j| j.metainfo.is_none()) {
                    self.job_metadata(id, metainfo);
                }
            }
            EngineEvent::Activity { id, activity } => {
                let Some(job) = self.jobs.get_mut(&id) else { return };
                if matches!(job.observed, Observed::InEngine(Phase::Metadata | Phase::Checking | Phase::Downloading))
                    && job.verify.is_none()
                {
                    job.observed = Observed::InEngine(match activity {
                        Activity::Metadata => Phase::Metadata,
                        Activity::Checking => Phase::Checking,
                        Activity::Downloading => Phase::Downloading,
                    });
                    self.publish(id);
                }
            }
            EngineEvent::Stats { id, stats } => {
                if let Some(job) = self.jobs.get_mut(&id) {
                    let now = Instant::now();
                    job.sample(stats, now);
                    if job.unsourced_for().is_some_and(|d| d >= self.config.stall_after) {
                        self.stop_unsourced(id);
                    } else {
                        self.publish(id);
                    }
                }
            }
            EngineEvent::Finished { id } => {
                let Some(job) = self.jobs.get_mut(&id) else { return };
                if job.record.intent == Intent::Running
                    && job.verify.is_none()
                    && matches!(
                        job.observed,
                        Observed::InEngine(Phase::Metadata | Phase::Checking | Phase::Downloading)
                    )
                {
                    job.observed = Observed::InEngine(Phase::Finishing { writes_stopped: false });
                    self.engine.send(EngineCommand::Pause(id));
                }
            }
            EngineEvent::Paused { id } => self.paused(id),
            EngineEvent::ResumeData { id, data, verified, piece_map, files } => {
                self.resume_data(id, data, verified, piece_map, files)
            }
            EngineEvent::Checked { id, damaged_pieces, verified, piece_map, files } => {
                self.checked(id, damaged_pieces, verified, piece_map, files)
            }
            EngineEvent::Failed { id, error } => {
                if self.probes.remove(&id) {
                    self.engine.send(EngineCommand::Remove(id));
                    self.draining.insert(id);
                    self.reply_metadata_waiters(id, Err(error));
                } else if self.jobs.get(&id).is_some_and(|j| j.observed.in_engine()) {
                    self.fail(id, error);
                }
            }
            EngineEvent::Removed { id } => {
                self.draining.remove(&id);
            }
        }
        self.finish_stop_if_done();
        self.schedule();
    }

    fn job_metadata(&mut self, id: TorrentId, metainfo: Metainfo) {
        let claimed = self
            .store
            .alias_owners(&metainfo.info_hash)
            .unwrap_or_default()
            .into_iter()
            .find(|owner| *owner != id && self.jobs.contains_key(owner));
        if let Some(original) = claimed {
            self.drop_duplicate(id, original);
            return;
        }
        if let Err(error) = self.store.learn(id, &metainfo.name, &metainfo.info_hash, &metainfo.bytes, now()) {
            self.fail(id, error);
            return;
        }
        let metainfo = Arc::new(metainfo);
        let job = self.jobs.get_mut(&id).expect("caller checked");
        job.record.name = Some(metainfo.name.clone());
        job.record.info_hash = metainfo.info_hash;
        job.metainfo = Some(metainfo.clone());
        let (dir, cap, selection) = (job.record.dir.clone(), job.record.cap, job.record.selection.clone());
        let admitted = admission::plan(&metainfo, &selection, &dir, None, self.disk.as_ref()).and_then(|plan| {
            admission::check_size(plan.selected_size, cap, Some(id))?;
            admission::check_conflicts(&plan)?;
            self.check_claims(id, &metainfo, &dir, &plan.wanted, None)?;
            admission::check_disk(plan.space, plan.selected_size, self.others_remaining(&dir, id))?;
            Ok(plan)
        });
        match admitted {
            Ok(plan) => {
                if self.jobs[&id].observed.in_engine() {
                    self.engine.send(EngineCommand::Select(id, plan.wanted));
                }
                self.publish(id);
                let info = self.job_files(id).expect("metadata just stored");
                self.reply_metadata_waiters(id, Ok(info));
            }
            Err(error) => {
                let info = self.job_files(id).expect("metadata just stored");
                self.reply_metadata_waiters(id, Ok(info));
                self.fail(id, error);
            }
        }
    }

    fn drop_duplicate(&mut self, id: TorrentId, original: TorrentId) {
        let _ = self.store.remove_duplicate(id);
        if let Some(job) = self.jobs.remove(&id)
            && job.observed.in_engine()
        {
            self.engine.send(EngineCommand::Remove(id));
            self.draining.insert(id);
        }
        let _ = self.updates.send(Update::Removed(id));
        let info = self.job_files(original);
        for mut waiter in std::mem::take(&mut self.metadata_waiters) {
            if waiter.id == id {
                waiter.id = original;
                if let Some(info) = &info {
                    let _ = waiter.reply.send(Ok(info.clone()));
                    continue;
                }
            }
            self.metadata_waiters.push(waiter);
        }
    }

    fn fail(&mut self, id: TorrentId, error: Error) {
        let Some(job) = self.jobs.get_mut(&id) else { return };
        job.verify = None;
        job.forget_sources();
        let saving = matches!(job.record.intent, Intent::Failed { .. })
            && matches!(job.observed, Observed::InEngine(Phase::Pausing | Phase::Paused));
        // libtorrent counts hash-checked pieces even when their disk writes fail.
        let disk = matches!(error.code, ErrorCode::DiskFull | ErrorCode::StorageError);
        if !saving {
            let intent = Intent::Failed { error };
            let _ = self.store.set_intent(id, &intent);
            job.record.intent = intent;
        }
        let save = job.metainfo.is_some() && !saving && !disk;
        job.observed = match job.observed {
            Observed::InEngine(Phase::Paused | Phase::Finishing { writes_stopped: true }) if save => {
                self.engine.send(EngineCommand::SaveResume(id));
                Observed::InEngine(Phase::Paused)
            }
            Observed::InEngine(Phase::Pausing | Phase::Finishing { writes_stopped: false }) if save => {
                Observed::InEngine(Phase::Pausing)
            }
            Observed::InEngine(Phase::Downloading) if save => {
                self.engine.send(EngineCommand::Pause(id));
                Observed::InEngine(Phase::Pausing)
            }
            Observed::InEngine(_) => {
                self.engine.send(EngineCommand::Remove(id));
                self.draining.insert(id);
                if let Some(stopping) = &mut self.stopping {
                    stopping.waiting.remove(&id);
                }
                for pause in &mut self.pauses {
                    pause.waiting.remove(&id);
                }
                Observed::Absent
            }
            outside @ (Observed::Absent | Observed::Parked) => outside,
        };
        self.publish(id);
    }

    fn stop_unsourced(&mut self, id: TorrentId) {
        let job = &self.jobs[&id];
        let pieces = job.stats.missing.map_or(0, |m| m.pieces) as usize;
        let verified = job.verified();
        let missing = job.selected_size().unwrap_or(verified).saturating_sub(verified);
        let size = |bytes| crate::output::format::size(Some(bytes));
        let error = Error::new(
            ErrorCode::NoSource,
            format!(
                "no peer or web seed has had the {} still missing ({}) for {}",
                crate::output::format::count(pieces, "piece"),
                size(missing),
                crate::output::format::elapsed(self.config.stall_after),
            ),
        )
        .with_hint(format!(
            "torrential resume {} looks for them again; the {} verified stays",
            &id.to_hex()[..6],
            size(verified)
        ))
        .with_next(format!("torrential resume {}", &id.to_hex()[..6]));
        self.fail(id, error);
    }

    fn paused(&mut self, id: TorrentId) {
        let Some(job) = self.jobs.get_mut(&id) else { return };
        match job.observed {
            Observed::InEngine(Phase::Finishing { writes_stopped: false }) => {
                job.observed = Observed::InEngine(Phase::Finishing { writes_stopped: true });
            }
            Observed::InEngine(Phase::Finishing { writes_stopped: true }) => return,
            Observed::InEngine(Phase::Checking) if job.verify.is_some() => return,
            Observed::InEngine(_) => job.observed = Observed::InEngine(Phase::Paused),
            Observed::Absent | Observed::Parked => return,
        }
        self.engine.send(EngineCommand::SaveResume(id));
        for pause in &mut self.pauses {
            pause.waiting.remove(&id);
        }
        self.finish_pauses();
        self.publish(id);
    }

    fn finish_pauses(&mut self) {
        let now = Instant::now();
        let (done, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.pauses).into_iter().partition(|p| p.waiting.is_empty() || p.deadline <= now);
        self.pauses = rest;
        for pause in done {
            let reply =
                if pause.waiting.is_empty() { Ok(pause.changes) } else { Err(self.still_writing(&pause.waiting)) };
            let _ = pause.reply.send(reply);
        }
    }

    fn still_writing(&self, ids: &BTreeSet<TorrentId>) -> Error {
        let shorts: Vec<String> = ids.iter().map(|id| id.to_hex()[..6].to_string()).collect();
        let within = crate::output::format::elapsed(self.config.writes_stop_within);
        Error::new(ErrorCode::JobConflict, format!("writes to {} did not stop within {within}", shorts.join(", ")))
            .with_hint(format!("run torrential pause {} again; it returns once writes stop", shorts.join(" ")))
            .with_next(format!("torrential pause {}", shorts.join(" ")))
    }

    fn resume_data(
        &mut self,
        id: TorrentId,
        data: Vec<u8>,
        verified: Option<u64>,
        piece_map: Option<PieceMap>,
        files: Option<Vec<u64>>,
    ) {
        let job = self.jobs.get(&id);
        let last = job.is_none_or(|j| {
            matches!(j.observed, Observed::InEngine(Phase::Paused | Phase::Finishing { writes_stopped: true }))
        });
        if last && let Some(stopping) = &mut self.stopping {
            stopping.waiting.remove(&id);
        }
        let Some(job) = job else { return };
        let verified = verified.unwrap_or_else(|| job.verified());
        let piece_map = piece_map.or_else(|| job.piece_map());
        let files = files.or_else(|| job.record.files_verified.clone());
        let finishing = job.observed == Observed::InEngine(Phase::Finishing { writes_stopped: true });
        if finishing && job.selected_size() == Some(verified) {
            let finished_at = now();
            if let Err(error) = self.store.finish(id, finished_at, &data, verified) {
                self.fail(id, error);
                return;
            }
            let job = self.jobs.get_mut(&id).expect("checked above");
            job.record.intent = Intent::Done { finished_at };
            job.record.verified = verified;
            job.observed = Observed::Absent;
            self.engine.send(EngineCommand::Remove(id));
            self.draining.insert(id);
            self.publish(id);
            return;
        }
        let saved = self.store.save_resume(id, &data, verified, piece_map, files.as_deref(), now()).is_ok();
        let job = self.jobs.get_mut(&id).expect("checked above");
        if saved {
            job.record.verified = verified;
            job.record.piece_map = piece_map;
            job.record.files_verified = files;
        }
        if finishing {
            job.observed = Observed::InEngine(Phase::Paused);
        } else if job.held_for_save() {
            job.observed = Observed::Absent;
            job.forget_sources();
            self.engine.send(EngineCommand::Remove(id));
            self.draining.insert(id);
            self.publish(id);
        }
    }

    fn checked(&mut self, id: TorrentId, damaged: u32, verified: u64, piece_map: Option<PieceMap>, files: Vec<u64>) {
        let Some(job) = self.jobs.get_mut(&id) else { return };
        let Some(mode) = job.verify.take() else { return };
        job.record.verified = verified;
        job.stats.verified = verified;
        job.record.piece_map = piece_map;
        job.stats.piece_map = piece_map;
        job.record.files_verified = Some(files);
        self.engine.send(EngineCommand::SaveResume(id));
        let short = id.to_hex()[..6].to_string();
        let intent = job.record.intent.clone();
        let files = job.record.files_verified.clone().unwrap_or_default();
        let record = |store: &mut Store, intent: Option<&Intent>| {
            let _ = store.record_check(id, intent, verified, piece_map, &files);
        };
        match (intent, damaged, mode) {
            (Intent::Running, n, _) => {
                if n > 0 {
                    record(&mut self.store, None);
                }
                job.observed = Observed::InEngine(Phase::Downloading);
                self.engine.send(EngineCommand::Resume(id));
            }
            (Intent::Paused, n, _) => {
                if n > 0 {
                    record(&mut self.store, None);
                }
                job.observed = Observed::InEngine(Phase::Paused);
            }
            (Intent::Done { .. } | Intent::Failed { .. }, 0, _) => {
                let finished_at = match job.record.intent {
                    Intent::Done { finished_at } => finished_at,
                    _ => now(),
                };
                let done = Intent::Done { finished_at };
                let _ = self.store.set_intent(id, &done);
                let job = self.jobs.get_mut(&id).expect("checked above");
                job.record.intent = done;
                job.observed = Observed::Absent;
                self.engine.send(EngineCommand::Remove(id));
                self.draining.insert(id);
            }
            (Intent::Done { .. } | Intent::Failed { .. }, _, Verify::Repair) => {
                record(&mut self.store, Some(&Intent::Running));
                job.record.intent = Intent::Running;
                job.observed = Observed::InEngine(Phase::Downloading);
                self.engine.send(EngineCommand::Resume(id));
            }
            (Intent::Failed { .. }, _, Verify::Report) => {
                record(&mut self.store, None);
                job.observed = Observed::InEngine(Phase::Paused);
            }
            (Intent::Done { .. }, n, Verify::Report) => {
                let error = Error::new(
                    ErrorCode::VerificationFailed,
                    format!("{} in the selected files", crate::output::format::count(n as usize, "damaged piece")),
                )
                .with_hint(format!("torrential verify {short} --repair downloads them again"))
                .with_next(format!("torrential verify {short} --repair"));
                let failed = Intent::Failed { error };
                record(&mut self.store, Some(&failed));
                job.record.intent = failed;
                job.observed = Observed::InEngine(Phase::Paused);
            }
        }
        self.publish(id);
    }

    fn schedule(&mut self) {
        if self.stopping.is_some() {
            return;
        }
        let ready = self.jobs.values().filter(|j| !self.draining.contains(&j.id()));
        for id in book::to_start(ready, self.config.max_active) {
            self.start(id);
        }
    }

    fn start(&mut self, id: TorrentId) {
        self.jobs.get_mut(&id).expect("scheduled").forget_sources();
        if let Err(error) = self.check_writes(id, "resume") {
            self.fail(id, error);
            return;
        }
        let job = &self.jobs[&id];
        if job.observed == Observed::InEngine(Phase::Paused) {
            self.engine.send(EngineCommand::Resume(id));
            self.jobs.get_mut(&id).expect("scheduled").observed = Observed::InEngine(Phase::Downloading);
            self.publish(id);
            return;
        }
        if job.metainfo.is_some()
            && let Ok(space) = self.disk.space(&job.record.dir)
            && let Err(error) =
                admission::check_disk(space, job.remaining(), self.others_remaining(&job.record.dir, id))
        {
            self.fail(id, error);
            return;
        }
        let resume_data = self.store.resume(id).ok().flatten();
        let job = &self.jobs[&id];
        let source = match (&job.metainfo, &job.record.magnet) {
            (Some(m), _) => AddSource::Metainfo(m.bytes.clone()),
            (None, Some(uri)) => AddSource::Magnet(uri.clone()),
            (None, None) => AddSource::Magnet(magnet::from_hash(job.record.info_hash).uri),
        };
        let phase = if job.metainfo.is_some() { Phase::Checking } else { Phase::Metadata };
        self.engine.send(EngineCommand::Add(AddTorrent {
            id,
            source,
            save_path: job.record.dir.clone(),
            wanted: job.wanted(),
            resume_data,
            paused: false,
        }));
        self.jobs.get_mut(&id).expect("scheduled").observed = Observed::InEngine(phase);
        self.publish(id);
    }

    fn check_writes(&self, id: TorrentId, command: &str) -> Result<(), Error> {
        let job = &self.jobs[&id];
        let (Some(metainfo), Some(wanted)) = (job.metainfo.as_deref(), job.wanted()) else { return Ok(()) };
        let (dir, retry) = (&job.record.dir, format!("torrential {command} {}", &id.to_hex()[..6]));
        admission::check_links(&admission::links(metainfo, dir, &wanted, self.disk.as_ref()), dir, Some(&retry))?;
        self.check_claims(id, metainfo, dir, &wanted, Some(&retry))
    }

    fn check_claims(
        &self,
        id: TorrentId,
        metainfo: &Metainfo,
        dir: &Path,
        wanted: &[bool],
        retry: Option<&str>,
    ) -> Result<(), Error> {
        let paths: BTreeMap<String, PathBuf> =
            files::targets(metainfo, dir, wanted).into_iter().map(|p| (claim_key(&p), p)).collect();
        for job in self.jobs.values().filter(|j| j.id() != id && !matches!(j.record.intent, Intent::Failed { .. })) {
            let (Some(theirs), Some(their_wanted)) = (job.metainfo.as_deref(), job.wanted()) else { continue };
            let taken: Vec<PathBuf> = files::targets(theirs, &job.record.dir, &their_wanted)
                .iter()
                .filter_map(|p| paths.get(&claim_key(p)).cloned())
                .collect();
            if !taken.is_empty() {
                let destination = dir.join(&metainfo.name);
                return Err(admission::claimed(&destination, job.id(), &theirs.name, &taken, retry));
            }
        }
        Ok(())
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let (expired, waiting): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.metadata_waiters).into_iter().partition(|w| w.deadline <= now);
        self.metadata_waiters = waiting;
        for waiter in expired {
            let short = waiter.id.to_hex()[..6].to_string();
            let _ = waiter.reply.send(Err(Error::new(
                ErrorCode::MetadataTimeout,
                format!("no peer sent the file list for {short} in time"),
            )
            .with_hint(format!("try again later, or run torrential get {short} to keep looking in the background"))));
        }
        let abandoned: Vec<TorrentId> =
            self.probes.iter().copied().filter(|id| !self.metadata_waiters.iter().any(|w| w.id == *id)).collect();
        for id in abandoned {
            self.probes.remove(&id);
            self.engine.send(EngineCommand::Remove(id));
            self.draining.insert(id);
        }
        self.finish_pauses();
        self.finish_stop_if_done();
        self.check_idle(now);
        self.save_progress(now);
    }

    fn save_progress(&mut self, now: Instant) {
        if self.stopping.is_some() || now.duration_since(self.resume_saved_at) < self.config.save_resume_every {
            return;
        }
        self.resume_saved_at = now;
        for job in self.jobs.values() {
            if job.record.intent == Intent::Running && job.observed == Observed::InEngine(Phase::Downloading) {
                self.engine.send(EngineCommand::SaveResumeIfProgressed(job.id()));
            }
        }
    }

    fn busy(&self) -> bool {
        self.jobs.values().any(|j| {
            j.observed.occupies_slot() || j.observed == Observed::InEngine(Phase::Pausing) || j.verify.is_some()
        }) || !self.probes.is_empty()
            || !self.pauses.is_empty()
            || !self.draining.is_empty()
    }

    fn check_idle(&mut self, now: Instant) {
        let Some(timeout) = self.config.idle_timeout else { return };
        if self.connections > 0 || self.busy() || self.stopping.is_some() {
            self.idle_since = now;
        } else if now.duration_since(self.idle_since) >= timeout && self.keep_until.is_none_or(|until| now >= until) {
            self.begin_stop(None);
        }
    }

    fn begin_stop(&mut self, reply: Option<oneshot::Sender<()>>) {
        if let Some(stopping) = &mut self.stopping {
            stopping.replies.extend(reply);
            return;
        }
        let mut waiting = BTreeSet::new();
        for job in self.jobs.values_mut() {
            let id = job.record.id;
            match job.observed {
                Observed::InEngine(Phase::Paused) => {
                    self.engine.send(EngineCommand::SaveResume(id));
                    waiting.insert(id);
                }
                Observed::InEngine(Phase::Pausing | Phase::Finishing { .. }) => {
                    waiting.insert(id);
                }
                Observed::InEngine(_) => {
                    job.observed = Observed::InEngine(Phase::Pausing);
                    self.engine.send(EngineCommand::Pause(id));
                    waiting.insert(id);
                }
                Observed::Absent | Observed::Parked => {}
            }
        }
        self.stopping = Some(Stopping {
            waiting,
            deadline: Instant::now() + self.config.writes_stop_within,
            replies: reply.into_iter().collect(),
        });
        self.finish_stop_if_done();
    }

    fn finish_stop_if_done(&mut self) {
        let Some(stopping) = &self.stopping else { return };
        if !stopping.waiting.is_empty() && Instant::now() < stopping.deadline {
            return;
        }
        let stopping = self.stopping.take().expect("checked above");
        for reply in stopping.replies {
            let _ = reply.send(());
        }
        self.stopped = true;
    }
}

// Default macOS filesystems ignore case.
fn claim_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

fn no_download(reference: &str) -> Error {
    Error::new(ErrorCode::NotFound, format!("no download matches {reference}"))
        .with_hint("run torrential status to list downloads")
        .with_next("torrential status")
}
