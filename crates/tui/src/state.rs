use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use torrential_core::units::format_limit;
use torrential_core::{
    Error, Job, JobStatus, Machine, Playback, Preferences, Release, ReleaseId, Screen, SourceReport, TorrentFile,
    TorrentId,
};

use crate::tree::Tree;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Item {
    Download(TorrentId),
    Result(ReleaseId),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Query,
    List,
    Files,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Engine {
    Starting,
    Ready,
    Unreachable(Error),
}

#[derive(Clone, PartialEq, Debug)]
pub enum SearchPhase {
    Running { pending: Vec<String> },
    Done,
    Failed(Error),
}

#[derive(Clone, PartialEq, Debug)]
pub struct Search {
    pub id: u64,
    pub text: String,
    pub started: Instant,
    pub releases: Vec<Release>,
    pub sources: Vec<SourceReport>,
    pub elapsed_ms: u32,
    pub phase: SearchPhase,
    pub ranked_for: Option<RankedFor>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RankedFor {
    pub profile: String,
    pub overrides: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    Screen,
    MaxSize,
    Playback,
}

pub const ROWS: [Row; 3] = [Row::Screen, Row::MaxSize, Row::Playback];

const CAPS: [u64; 3] = [2 << 30, 8 << 30, 20 << 30];

impl Row {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::MaxSize => "max size",
            Self::Playback => "playback",
        }
    }

    pub fn value(self, preferences: Preferences) -> String {
        match self {
            Self::Screen => preferences.screen.map_or("auto", Screen::as_str).into(),
            Self::MaxSize => preferences.max_size.map_or_else(|| "none".into(), format_limit),
            Self::Playback => preferences.playback.map_or("auto", Playback::as_str).into(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ranking {
    pub row: Row,
    pub picks: Preferences,
    pub saved: Preferences,
}

impl Ranking {
    pub fn new(saved: Preferences) -> Self {
        Self { row: Row::Screen, picks: saved, saved }
    }

    pub fn edit(&self) -> Edit {
        Edit { from: self.saved, to: self.picks }
    }

    pub fn choices(&self, row: Row) -> Vec<Preferences> {
        let with = self.picks;
        match row {
            Row::Screen => [None, Some(Screen::Phone), Some(Screen::Laptop), Some(Screen::Tv), Some(Screen::Tv4k)]
                .map(|screen| Preferences { screen, ..with })
                .to_vec(),
            Row::MaxSize => {
                let mut caps: Vec<u64> = CAPS.into_iter().chain(self.saved.max_size).collect();
                caps.sort_unstable();
                caps.dedup();
                let caps = caps.into_iter().map(Some);
                std::iter::once(None).chain(caps).map(|max_size| Preferences { max_size, ..with }).collect()
            }
            Row::Playback => [None, Some(Playback::HevcHdr), Some(Playback::Hevc), Some(Playback::H264)]
                .map(|playback| Preferences { playback, ..with })
                .to_vec(),
        }
    }

    pub fn picked(&self, row: Row) -> usize {
        self.choices(row).iter().position(|choice| *choice == self.picks).unwrap_or(0)
    }

    pub fn choose(&mut self, delta: isize) {
        let choices = self.choices(self.row);
        self.picks = choices[self.picked(self.row).saturating_add_signed(delta).min(choices.len() - 1)];
    }

    pub fn move_row(&mut self, delta: isize) {
        let at = ROWS.iter().position(|row| *row == self.row).unwrap_or(0);
        self.row = ROWS[at.saturating_add_signed(delta).min(ROWS.len() - 1)];
    }
}

/// What a save changes: the rows whose pick moved from what the panel opened on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Edit {
    pub from: Preferences,
    pub to: Preferences,
}

impl Edit {
    /// `current` with this edit's changed rows, so a save keeps every row the panel left alone,
    /// even one it opened on before the overrides were read, or one saved since.
    pub fn apply(&self, current: Preferences) -> Preferences {
        fn row<T: PartialEq>(from: T, to: T, current: T) -> T {
            if from == to { current } else { to }
        }
        Preferences {
            screen: row(self.from.screen, self.to.screen, current.screen),
            max_size: row(self.from.max_size, self.to.max_size, current.max_size),
            playback: row(self.from.playback, self.to.playback, current.playback),
        }
    }

    /// Each changed row with its new value, as `screen tv-4k` or `max size none`.
    pub fn changes(&self) -> Vec<String> {
        let changed = ROWS.into_iter().filter(|row| row.value(self.from) != row.value(self.to));
        changed.map(|row| format!("{} {}", row.name(), row.value(self.to))).collect()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    BestMatch,
    Seeds,
    Size,
    Age,
}

impl Sort {
    pub fn label(self) -> &'static str {
        match self {
            Self::BestMatch => "best match",
            Self::Seeds => "seeds",
            Self::Size => "size",
            Self::Age => "age",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::BestMatch => Self::Seeds,
            Self::Seeds => Self::Size,
            Self::Size => Self::Age,
            Self::Age => Self::BestMatch,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Input {
    pub text: String,
    pub caret: usize,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Files {
    Loading { since: Instant, then_download: bool },
    Ready(Tree),
    Failed(Error),
}

#[derive(Clone, PartialEq, Debug)]
pub struct DownloadFiles {
    pub files: Vec<TorrentFile>,
    pub asked: Instant,
    pub cursor: usize,
    pub scroll: usize,
}

impl DownloadFiles {
    pub fn new(asked: Instant) -> Self {
        Self { files: Vec::new(), asked, cursor: 0, scroll: 0 }
    }

    pub fn picked(&self) -> Vec<&TorrentFile> {
        self.files.iter().filter(|f| f.selected).collect()
    }

    pub fn move_by(&mut self, delta: isize) {
        self.cursor = self.cursor.saturating_add_signed(delta);
    }

    pub fn settle(&mut self, rows: usize) {
        let count = self.picked().len();
        self.cursor = self.cursor.min(count.saturating_sub(1));
        let context = usize::from(rows > 2);
        if self.cursor < self.scroll + context {
            self.scroll = self.cursor.saturating_sub(context);
        } else if rows > 0 && self.cursor + context >= self.scroll + rows {
            self.scroll = self.cursor + context + 1 - rows;
        }
        self.scroll = self.scroll.min(count.saturating_sub(rows.max(1)));
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobAction {
    Pause,
    Resume,
    Verify,
    Remove,
}

impl JobAction {
    pub fn progressive(self) -> &'static str {
        match self {
            Self::Pause => "pausing",
            Self::Resume => "resuming",
            Self::Verify => "verifying",
            Self::Remove => "removing",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Toast {
    pub text: String,
    pub tone: Tone,
    pub until: Instant,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Upgrade {
    Available(String),
    Installing(String),
    Installed,
}

pub fn group(status: &JobStatus) -> u8 {
    match status {
        JobStatus::Downloading | JobStatus::Checking | JobStatus::Metadata | JobStatus::Queued => 0,
        JobStatus::Error { .. } | JobStatus::Interrupted => 1,
        JobStatus::Paused => 2,
        JobStatus::Done { .. } => 3,
    }
}

pub fn label(status: &JobStatus) -> &'static str {
    match status {
        JobStatus::Downloading | JobStatus::Metadata => "getting",
        JobStatus::Checking => "checking",
        JobStatus::Queued => "waiting",
        JobStatus::Error { .. } => "error",
        JobStatus::Interrupted => "stopped",
        JobStatus::Paused => "paused",
        JobStatus::Done { .. } => "done",
    }
}

pub fn folds(status: &JobStatus) -> bool {
    matches!(status, JobStatus::Paused | JobStatus::Done { .. })
}

pub struct State {
    pub now: Instant,
    pub clock: i64,
    pub started: Instant,
    pub size: (u16, u16),
    pub home: Option<PathBuf>,
    pub engine: Engine,
    pub downloads: Vec<Job>,
    pub download_files: HashMap<TorrentId, DownloadFiles>,
    pub pending: HashMap<TorrentId, JobAction>,
    pub arrived: HashMap<TorrentId, Instant>,
    pub free_space: Option<u64>,
    pub free_space_at: Option<Instant>,
    pub query: Input,
    pub search: Option<Search>,
    pub sort: Sort,
    pub files: HashMap<ReleaseId, Files>,
    pub focus: Focus,
    pub cursor: Option<Item>,
    pub collapsed: bool,
    pub follow_top: bool,
    pub touched: bool,
    pub prefetch: Option<(ReleaseId, Instant)>,
    pub scroll: u16,
    pub help: bool,
    pub confirm: Option<TorrentId>,
    pub ranking: Option<Ranking>,
    pub preferences: Option<Preferences>,
    pub machine: Option<Machine>,
    pub toast: Option<Toast>,
    pub upgrade: Option<Upgrade>,
    pub quit: bool,
    pub next_search: u64,
}

pub const SPINNER: [&str; 4] = ["   ", ".  ", ".. ", "..."];
pub const SPIN_EVERY: Duration = Duration::from_millis(250);
pub const STALL_AFTER: Duration = Duration::from_secs(30);
pub const PREFETCH_AFTER: Duration = Duration::from_millis(300);

impl State {
    pub fn new(now: Instant, clock: i64, size: (u16, u16), home: Option<PathBuf>) -> Self {
        Self {
            now,
            clock,
            started: now,
            size,
            home,
            engine: Engine::Starting,
            downloads: Vec::new(),
            download_files: HashMap::new(),
            pending: HashMap::new(),
            arrived: HashMap::new(),
            free_space: None,
            free_space_at: None,
            query: Input::default(),
            search: None,
            sort: Sort::BestMatch,
            files: HashMap::new(),
            focus: Focus::Query,
            cursor: None,
            collapsed: false,
            follow_top: false,
            touched: false,
            prefetch: None,
            scroll: 0,
            help: false,
            confirm: None,
            ranking: None,
            preferences: None,
            machine: None,
            toast: None,
            upgrade: None,
            quit: false,
            next_search: 1,
        }
    }

    fn frames(&self) -> u128 {
        self.now.duration_since(self.started).as_millis() / SPIN_EVERY.as_millis()
    }

    pub fn spinner(&self) -> &'static str {
        SPINNER[self.frames() as usize % SPINNER.len()]
    }

    pub fn next_frame(&self) -> Instant {
        self.started + SPIN_EVERY * (self.frames() as u32 + 1)
    }

    pub fn job(&self, id: TorrentId) -> Option<&Job> {
        self.downloads.iter().find(|j| j.id == id)
    }

    pub fn stalled(&self, job: &Job) -> bool {
        self.quiet(job).is_some_and(|at| at + STALL_AFTER <= self.now)
    }

    pub fn getting(&self, job: &Job) -> bool {
        job.status == JobStatus::Downloading && !self.stalled(job)
    }

    pub fn next_stall(&self) -> Option<Instant> {
        self.downloads.iter().filter_map(|j| self.quiet(j)).map(|at| at + STALL_AFTER).filter(|at| *at > self.now).min()
    }

    fn quiet(&self, job: &Job) -> Option<Instant> {
        let waiting = job.status == JobStatus::Downloading && job.progress.down_rate == 0;
        self.arrived.get(&job.id).copied().filter(|_| waiting)
    }

    pub fn note_arrival(&mut self, job: &Job) {
        let was_downloading = self.job(job.id).is_some_and(|j| j.status == JobStatus::Downloading);
        if job.progress.down_rate > 0 || !was_downloading {
            self.arrived.insert(job.id, self.now);
        }
    }

    pub fn release(&self, id: ReleaseId) -> Option<&Release> {
        self.search.as_ref()?.releases.iter().find(|r| r.id == id)
    }

    pub fn filtering(&self) -> bool {
        let typed = self.query.text.trim();
        !typed.is_empty() && self.search.as_ref().is_some_and(|s| s.text != typed)
    }

    pub fn results(&self) -> Vec<&Release> {
        let Some(search) = &self.search else { return Vec::new() };
        let words: Vec<String> = if self.filtering() {
            self.query.text.split_whitespace().map(str::to_lowercase).collect()
        } else {
            Vec::new()
        };
        let mut shown: Vec<&Release> = search
            .releases
            .iter()
            .filter(|r| words.iter().all(|w| r.name.to_lowercase().contains(w.as_str())))
            .collect();
        match self.sort {
            Sort::BestMatch => {}
            Sort::Seeds => shown.sort_by_key(|r| std::cmp::Reverse(r.seeders)),
            Sort::Size => shown.sort_by_key(|r| std::cmp::Reverse(r.size)),
            Sort::Age => shown.sort_by_key(|r| std::cmp::Reverse(r.published)),
        }
        shown
    }

    pub fn in_downloads(&self) -> bool {
        matches!(self.cursor, Some(Item::Download(_))) && self.focus != Focus::Query
    }

    pub fn download_rows(&self) -> Vec<&Job> {
        let unfold = self.in_downloads();
        self.downloads.iter().filter(|j| unfold || !folds(&j.status)).collect()
    }

    pub fn folded(&self) -> Option<(usize, usize)> {
        if self.in_downloads() {
            return None;
        }
        let paused = self.downloads.iter().filter(|j| j.status == JobStatus::Paused).count();
        let done = self.downloads.iter().filter(|j| matches!(j.status, JobStatus::Done { .. })).count();
        (paused + done > 0).then_some((paused, done))
    }

    pub fn items(&self) -> Vec<Item> {
        let downloads = self.downloads.iter().map(|j| Item::Download(j.id));
        downloads.chain(self.results().into_iter().map(|r| Item::Result(r.id))).collect()
    }

    pub fn active(&self) -> usize {
        self.downloads
            .iter()
            .filter(|j| matches!(j.status, JobStatus::Downloading | JobStatus::Metadata | JobStatus::Checking))
            .count()
    }

    pub fn rate(&self) -> u64 {
        self.downloads.iter().map(|j| j.progress.down_rate).sum()
    }

    pub fn resort(&mut self) {
        self.downloads.sort_by_key(|j| (group(&j.status), std::cmp::Reverse(j.added_at)));
    }

    pub fn spinning(&self) -> bool {
        let starting = self.engine == Engine::Starting;
        let searching = self.search.as_ref().is_some_and(|s| matches!(s.phase, SearchPhase::Running { .. }));
        let loading = self.files.values().any(|f| matches!(f, Files::Loading { .. }));
        let busy_job = self.downloads.iter().any(|j| matches!(j.status, JobStatus::Metadata | JobStatus::Checking));
        let upgrading = matches!(self.upgrade, Some(Upgrade::Installing(_)));
        starting || searching || loading || busy_job || upgrading || !self.pending.is_empty()
    }

    pub fn say(&mut self, tone: Tone, text: impl Into<String>) {
        let lasts = if tone == Tone::Error { Duration::from_secs(8) } else { Duration::from_secs(5) };
        self.toast = Some(Toast { text: text.into(), tone, until: self.now + lasts });
    }
}
