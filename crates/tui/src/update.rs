use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use torrential_core::protocol::{Change, Event, FilesReply, GetOutcome, GetReply};
use torrential_core::text::sanitize;
use torrential_core::units::count;
use torrential_core::{
    Arch, Error, Job, JobStatus, Machine, Preferences, ReleaseId, SearchQuery, SearchResult, TorrentId,
};

use crate::format::{first_fit, home_path};
use crate::layout;
use crate::state::{
    DownloadFiles, Edit, Engine, Files, Focus, Input, Item, JobAction, RankedFor, Ranking, Search, SearchPhase, State,
    Tone, Upgrade,
};
use crate::tree::Tree;

#[derive(Debug)]
pub enum Msg {
    Key(KeyEvent),
    Resize(u16, u16),
    Tick,
    Watch(Event),
    WatchLost(Error),
    FreeSpace(Option<u64>),
    SearchUpdate { search: u64, result: SearchResult, pending: Vec<String> },
    SearchDone { search: u64, result: Result<SearchResult, Error> },
    Files { item: Item, result: Result<FilesReply, Error> },
    Started { release: ReleaseId, result: Result<Box<GetReply>, Error> },
    Changed { id: TorrentId, action: JobAction, result: Result<Vec<Change>, Error> },
    Preferences(Preferences),
    Machine(Machine),
    Saved { edit: Edit, result: Result<Preferences, Error> },
    NewerVersion(String),
    Upgraded(Result<Option<String>, Error>),
}

#[derive(Clone, PartialEq, Debug)]
pub enum Effect {
    Search { search: u64, query: SearchQuery },
    Files(Item),
    Get { release: ReleaseId, files: Option<Vec<String>> },
    Job { id: TorrentId, action: JobAction },
    FreeSpace,
    Open(PathBuf),
    Copy(String),
    SavePreferences(Edit),
    Upgrade,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Quit,
    Help,
    Search,
    Downloads,
    Results,
    NextSection,
    PreviousSection,
    Back,
    Move(Move),
    Open,
    Collapse,
    Download,
    Sort,
    TogglePause,
    Verify,
    Remove,
    OpenFolder,
    CopyPath,
    Pick,
    PickAll,
    Ranking,
    Upgrade,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Move {
    Up,
    Down,
    PageUp,
    PageDown,
    First,
    Last,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Query,
    Downloads,
    Results,
}

const RESULT_LIMIT: u16 = 50;

impl State {
    pub fn update(&mut self, msg: Msg) -> Vec<Effect> {
        let before = self.anchor();
        if self.toast.as_ref().is_some_and(|t| t.until <= self.now) {
            self.toast = None;
        }
        let mut effects = self.prefetch_due();
        effects.extend(match msg {
            Msg::Key(key) => self.key(key),
            Msg::Resize(w, h) => {
                self.size = (w, h);
                Vec::new()
            }
            Msg::Tick => Vec::new(),
            Msg::Watch(event) => self.watch(event),
            Msg::WatchLost(error) => {
                if !matches!(self.engine, Engine::Unreachable(_)) {
                    self.say(Tone::Error, format!("Cannot reach the engine: {}", error.message));
                }
                self.engine = Engine::Unreachable(error);
                Vec::new()
            }
            Msg::FreeSpace(free) => {
                self.free_space = free;
                Vec::new()
            }
            Msg::SearchUpdate { search, result, pending } => {
                self.merge(search, result, SearchPhase::Running { pending });
                Vec::new()
            }
            Msg::SearchDone { search, result } => {
                match result {
                    Ok(result) => self.merge(search, result, SearchPhase::Done),
                    Err(error) => {
                        if let Some(s) = self.search.as_mut().filter(|s| s.id == search) {
                            s.phase = SearchPhase::Failed(error);
                        }
                    }
                }
                Vec::new()
            }
            Msg::Files { item, result } => self.files_arrived(item, result),
            Msg::Started { release, result } => self.started(release, result),
            Msg::Changed { id, action, result } => self.changed(id, action, result),
            Msg::Preferences(preferences) => {
                self.preferences.get_or_insert(preferences);
                Vec::new()
            }
            Msg::Machine(machine) => {
                self.machine.get_or_insert(machine);
                Vec::new()
            }
            Msg::Saved { edit, result } => self.saved(edit, result),
            Msg::NewerVersion(latest) => {
                self.upgrade.get_or_insert(Upgrade::Available(latest));
                Vec::new()
            }
            Msg::Upgraded(result) => {
                self.upgraded(result);
                Vec::new()
            }
        });
        self.plan_prefetch();
        self.settle(before);
        effects
    }

    fn plan_prefetch(&mut self) {
        let wanted = match self.cursor {
            Some(Item::Result(id))
                if self.focus != Focus::Query
                    && !self.files.contains_key(&id)
                    && self.release(id).is_some_and(|r| r.torrent_url.is_some()) =>
            {
                Some(id)
            }
            _ => None,
        };
        match (wanted, self.prefetch) {
            (Some(id), Some((planned, _))) if planned == id => {}
            (Some(id), _) => self.prefetch = Some((id, self.now + crate::state::PREFETCH_AFTER)),
            (None, _) => self.prefetch = None,
        }
    }

    fn prefetch_due(&mut self) -> Vec<Effect> {
        match self.prefetch {
            Some((id, at)) if at <= self.now => {
                self.prefetch = None;
                self.files.insert(id, Files::Loading { since: self.now, then_download: false });
                vec![Effect::Files(Item::Result(id))]
            }
            _ => Vec::new(),
        }
    }

    fn anchor(&self) -> Option<(Item, i32)> {
        let item = self.cursor.filter(|_| self.ranking.is_none())?;
        let placed = layout::blocks(self, layout::list_height(self));
        let (top, _) = layout::span(&placed, item)?;
        Some((item, i32::from(top) - i32::from(self.scroll)))
    }

    fn settle(&mut self, before: Option<(Item, i32)>) {
        let viewport = layout::list_height(self);
        if self.focus == Focus::Files
            && let Some(Item::Result(id)) = self.cursor
            && let Some(Files::Ready(tree)) = self.files.get_mut(&id)
        {
            tree.settle(layout::tree_rows(tree.visible().len(), viewport));
        }
        if let Some(Item::Download(id)) = self.cursor
            && layout::in_files(self, id)
            && let Some(job) = self.job(id)
        {
            let (rows, _) = layout::download_file_rows(self, job, viewport);
            if let Some(files) = self.download_files.get_mut(&id) {
                files.settle(rows);
            }
        }
        let placed = layout::blocks(self, viewport);
        let mut scroll = i32::from(self.scroll);
        if let (Some((item, row)), Some(cursor)) = (before, self.cursor)
            && item == cursor
            && let Some((top, _)) = layout::span(&placed, cursor)
        {
            scroll = i32::from(top) - row;
        }
        let lines = match self.ranking {
            Some(_) => layout::ranking_span(&placed),
            None => self.cursor.and_then(|item| layout::span(&placed, item)),
        };
        self.scroll = layout::settle(&placed, lines, scroll, viewport);
    }

    fn key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Vec::new();
        }
        self.touched = true;
        self.follow_top = false;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = true;
            return Vec::new();
        }
        if let Some(id) = self.confirm {
            return self.confirm_key(id, key);
        }
        if let Some(ranking) = self.ranking {
            return self.ranking_key(ranking, key);
        }
        if key.code == KeyCode::F(1) {
            return self.act(Action::Help);
        }
        if self.focus == Focus::Query {
            return self.query_key(key);
        }
        match self.binding(key) {
            Some(action) => self.act(action),
            None => Vec::new(),
        }
    }

    fn binding(&self, key: KeyEvent) -> Option<Action> {
        use KeyCode::*;
        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return None;
        }
        let download = matches!(self.cursor, Some(Item::Download(_)));
        let picking = self.focus == Focus::Files && !download;
        Some(match key.code {
            Char('q') => Action::Quit,
            Char('?') => Action::Help,
            Char('/') => Action::Search,
            Char('d') => Action::Downloads,
            Char('r') => Action::Results,
            Tab => Action::NextSection,
            BackTab => Action::PreviousSection,
            Esc => Action::Back,
            Down | Char('j') => Action::Move(Move::Down),
            Up | Char('k') => Action::Move(Move::Up),
            PageDown => Action::Move(Move::PageDown),
            PageUp => Action::Move(Move::PageUp),
            Home | Char('g') => Action::Move(Move::First),
            End | Char('G') => Action::Move(Move::Last),
            Enter if picking && self.tree_ready() => Action::Download,
            Enter | Right | Char('l') => Action::Open,
            Left | Char('h') => Action::Collapse,
            Char(' ') if picking => Action::Pick,
            Char('a') if picking => Action::PickAll,
            Char('D') => Action::Download,
            Char('s') => Action::Sort,
            Char('S') => Action::Ranking,
            Char('U') => Action::Upgrade,
            Char(' ' | 'p') if download => Action::TogglePause,
            Char('v') if download => Action::Verify,
            Char('x') if download => Action::Remove,
            Char('o') if download => Action::OpenFolder,
            Char('y') if download => Action::CopyPath,
            _ => return None,
        })
    }

    pub fn act(&mut self, action: Action) -> Vec<Effect> {
        match action {
            Action::Quit => self.quit = true,
            Action::Help => self.help = !self.help,
            Action::Search => {
                self.focus = Focus::Query;
                self.query.caret = self.query.text.chars().count();
            }
            Action::Downloads => return self.enter(Section::Downloads),
            Action::Results => return self.enter(Section::Results),
            Action::NextSection => return self.cycle(1),
            Action::PreviousSection => return self.cycle(-1),
            Action::Back => self.back(),
            Action::Move(m) => return self.move_cursor(m),
            Action::Open => return self.open(),
            Action::Collapse => {
                let in_files = self.focus == Focus::Files;
                match self.tree() {
                    Some(tree) => tree.fold(),
                    None if in_files => self.focus = Focus::List,
                    None => self.collapsed = true,
                }
            }
            Action::Download => {
                if let Some(Item::Result(id)) = self.cursor {
                    return self.download(id);
                }
            }
            Action::Sort => {
                self.sort = self.sort.next();
                self.say(Tone::Info, format!("Sorted by {}", self.sort.label()));
            }
            Action::TogglePause => {
                if let Some(job) = self.selected_job() {
                    let action = match job.status {
                        JobStatus::Paused | JobStatus::Interrupted | JobStatus::Error { .. } => JobAction::Resume,
                        JobStatus::Done { .. } => {
                            self.say(Tone::Info, "This download is done.");
                            return Vec::new();
                        }
                        _ => JobAction::Pause,
                    };
                    return self.job_action(job.id, action);
                }
            }
            Action::Verify => {
                if let Some(job) = self.selected_job() {
                    return self.job_action(job.id, JobAction::Verify);
                }
            }
            Action::Remove => {
                if let Some(job) = self.selected_job() {
                    self.confirm = Some(job.id);
                }
            }
            Action::OpenFolder => {
                if let Some(job) = self.selected_job() {
                    let dir = job.dir.clone();
                    self.say(Tone::Info, format!("Opened {}", home_path(&dir, self.home.as_deref())));
                    return vec![Effect::Open(dir)];
                }
            }
            Action::CopyPath => {
                if let Some(job) = self.selected_job() {
                    let path = job.path.clone().unwrap_or_else(|| job.dir.clone());
                    self.say(Tone::Success, "Copied path");
                    return vec![Effect::Copy(path.display().to_string())];
                }
            }
            Action::Pick => {
                if let Some(tree) = self.tree() {
                    tree.toggle();
                }
            }
            Action::PickAll => {
                if let Some(tree) = self.tree() {
                    tree.toggle_all();
                }
            }
            Action::Ranking => {
                self.ranking = Some(Ranking::new(self.preferences.unwrap_or_default()));
                self.help = false;
                self.focus = Focus::List;
            }
            Action::Upgrade => {
                if let Some(Upgrade::Available(latest)) = self.upgrade.take() {
                    self.upgrade = Some(Upgrade::Installing(latest));
                    return vec![Effect::Upgrade];
                }
            }
        }
        Vec::new()
    }

    fn upgraded(&mut self, result: Result<Option<String>, Error>) {
        match result {
            Ok(Some(_)) => {
                self.upgrade = Some(Upgrade::Installed);
                self.quit = true;
            }
            Ok(None) => {
                self.upgrade = None;
                self.say(Tone::Info, "This torrential is the latest.");
            }
            Err(error) => {
                if let Some(Upgrade::Installing(latest)) = self.upgrade.take() {
                    self.upgrade = Some(Upgrade::Available(latest));
                }
                self.say(Tone::Error, format!("Updating failed: {}", error.message));
            }
        }
    }

    fn ranking_key(&mut self, mut ranking: Ranking, key: KeyEvent) -> Vec<Effect> {
        use KeyCode::*;
        match key.code {
            Char('q') => self.quit = true,
            Up | Char('k') => ranking.move_row(-1),
            Down | Char('j') => ranking.move_row(1),
            Left | Char('h') => ranking.choose(-1),
            Right | Char('l') => ranking.choose(1),
            Enter => {
                self.ranking = None;
                let changed = ranking.picks != ranking.saved;
                return if changed { vec![Effect::SavePreferences(ranking.edit())] } else { Vec::new() };
            }
            Esc | Char('S') => {
                self.ranking = None;
                return Vec::new();
            }
            _ => return Vec::new(),
        }
        self.ranking = Some(ranking);
        Vec::new()
    }

    fn saved(&mut self, edit: Edit, result: Result<Preferences, Error>) -> Vec<Effect> {
        let preferences = match result {
            Ok(preferences) => preferences,
            Err(error) => {
                self.say(Tone::Error, format!("Saving failed: {}", error.message));
                return Vec::new();
            }
        };
        self.preferences = Some(preferences);
        let changes = edit.changes();
        let listed = (1..=changes.len()).rev().map(|n| match changes.len() - n {
            0 => format!("Saved {}.", changes[..n].join(", ")),
            more => format!("Saved {} and {more} more.", changes[..n].join(", ")),
        });
        let room = layout::screen(self.size).map_or(0, |s| s.right - s.at.content - 16);
        let counted = format!("Saved {}.", count(changes.len(), "change"));
        self.say(Tone::Success, first_fit(listed.chain([counted]), room.into()).unwrap_or_else(|| "Saved.".into()));
        let Some(text) = self.search.as_ref().map(|s| s.text.clone()) else { return Vec::new() };
        let typed = self.filtering().then(|| self.query.clone());
        let effects = self.search_for(text);
        if let Some(typed) = typed {
            self.query = typed;
        }
        effects
    }

    fn selected_job(&self) -> Option<Job> {
        match self.cursor {
            Some(Item::Download(id)) => self.job(id).cloned(),
            _ => None,
        }
    }

    fn tree_ready(&self) -> bool {
        matches!(self.cursor, Some(Item::Result(id)) if matches!(self.files.get(&id), Some(Files::Ready(_))))
    }

    fn tree(&mut self) -> Option<&mut Tree> {
        let Some(Item::Result(id)) = self.cursor else { return None };
        match (self.focus, self.files.get_mut(&id)) {
            (Focus::Files, Some(Files::Ready(tree))) => Some(tree),
            _ => None,
        }
    }

    fn download_files(&mut self) -> Option<&mut DownloadFiles> {
        let Some(Item::Download(id)) = self.cursor else { return None };
        match self.focus {
            Focus::Files => self.download_files.get_mut(&id),
            _ => None,
        }
    }

    fn back(&mut self) {
        if self.help {
            self.help = false;
        } else if self.focus == Focus::Files {
            self.focus = Focus::List;
        } else {
            self.act(Action::Search);
        }
    }

    fn select(&mut self, item: Item) -> Vec<Effect> {
        self.focus = Focus::List;
        if self.cursor == Some(item) {
            return Vec::new();
        }
        self.cursor = Some(item);
        self.collapsed = false;
        self.follow_top = false;
        self.resort();
        match item {
            Item::Download(id) => self.refresh_files(id, true),
            Item::Result(_) => Vec::new(),
        }
    }

    fn move_cursor(&mut self, m: Move) -> Vec<Effect> {
        let delta = match m {
            Move::Up => -1,
            Move::Down => 1,
            Move::PageUp => -10,
            Move::PageDown => 10,
            Move::First => isize::MIN / 2,
            Move::Last => isize::MAX / 2,
        };
        if let Some(tree) = self.tree() {
            tree.move_by(delta);
            return Vec::new();
        }
        if let Some(files) = self.download_files() {
            files.move_by(delta);
            return Vec::new();
        }
        let items = self.items();
        if items.is_empty() {
            return Vec::new();
        }
        let at = self.cursor.and_then(|c| items.iter().position(|i| *i == c));
        let last = items.len() - 1;
        let to = match (m, at) {
            (Move::First, _) | (Move::Up | Move::PageUp, None) => 0,
            (Move::Last, _) => last,
            (Move::Down | Move::PageDown, None) => 0,
            (Move::Down, Some(at)) => (at + 1).min(last),
            (Move::Up, Some(at)) => at.saturating_sub(1),
            (Move::PageDown, Some(at)) => self.page(&items, at, 1),
            (Move::PageUp, Some(at)) => self.page(&items, at, -1),
        };
        self.select(items[to])
    }

    fn page(&self, items: &[Item], at: usize, direction: i32) -> usize {
        let viewport = layout::list_height(self);
        let placed = layout::blocks(self, viewport);
        let top = |item: &Item| layout::span(&placed, *item).map(|(top, _)| i32::from(top));
        let Some(from) = top(&items[at]) else { return at };
        let target = from + direction * i32::from(viewport.saturating_sub(2)).max(1);
        let rows: Vec<(usize, i32)> = items.iter().enumerate().filter_map(|(i, item)| Some((i, top(item)?))).collect();
        let found = if direction > 0 {
            rows.iter().rev().find(|(_, t)| *t <= target).map_or(at, |(i, _)| *i).max(at + 1)
        } else {
            rows.iter().find(|(_, t)| *t >= target).map_or(0, |(i, _)| *i).min(at.saturating_sub(1))
        };
        found.min(items.len() - 1)
    }

    fn sections(&self) -> Vec<Section> {
        let mut sections = vec![Section::Query];
        if !self.downloads.is_empty() {
            sections.push(Section::Downloads);
        }
        if !self.results().is_empty() {
            sections.push(Section::Results);
        }
        sections
    }

    fn section(&self) -> Section {
        match (self.focus, self.cursor) {
            (Focus::Query, _) | (_, None) => Section::Query,
            (_, Some(Item::Download(_))) => Section::Downloads,
            (_, Some(Item::Result(_))) => Section::Results,
        }
    }

    fn cycle(&mut self, step: isize) -> Vec<Effect> {
        let sections = self.sections();
        let at = sections.iter().position(|s| *s == self.section()).unwrap_or(0);
        let to = (at as isize + step).rem_euclid(sections.len() as isize) as usize;
        self.enter(sections[to])
    }

    fn enter(&mut self, section: Section) -> Vec<Effect> {
        match section {
            Section::Query => {
                self.act(Action::Search);
                Vec::new()
            }
            Section::Downloads => {
                if self.downloads.is_empty() {
                    self.say(Tone::Info, "No downloads yet. Press / to search.");
                    return Vec::new();
                }
                self.resort();
                let first = self.downloads[0].id;
                self.select(Item::Download(first))
            }
            Section::Results => match self.results().first().map(|r| r.id) {
                Some(id) => self.select(Item::Result(id)),
                None => {
                    self.say(Tone::Info, "No results yet. Press / to search.");
                    Vec::new()
                }
            },
        }
    }

    fn open(&mut self) -> Vec<Effect> {
        if let Some(tree) = self.tree() {
            tree.unfold();
            return Vec::new();
        }
        if self.collapsed {
            self.collapsed = false;
            return Vec::new();
        }
        match self.cursor {
            Some(Item::Result(id)) => self.open_files(id, false),
            Some(Item::Download(id)) => {
                if self.download_files.get(&id).is_some_and(|f| f.picked().len() > 1) {
                    self.focus = Focus::Files;
                }
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    fn open_files(&mut self, id: ReleaseId, then_download: bool) -> Vec<Effect> {
        self.collapsed = false;
        match self.files.get_mut(&id) {
            Some(Files::Ready(_)) => {
                self.focus = Focus::Files;
                Vec::new()
            }
            Some(Files::Loading { then_download: wants, .. }) => {
                *wants |= then_download;
                self.focus = if then_download { Focus::List } else { Focus::Files };
                Vec::new()
            }
            None | Some(Files::Failed(_)) => {
                self.files.insert(id, Files::Loading { since: self.now, then_download });
                self.focus = if then_download { Focus::List } else { Focus::Files };
                vec![Effect::Files(Item::Result(id))]
            }
        }
    }

    fn download(&mut self, id: ReleaseId) -> Vec<Effect> {
        let Some(Files::Ready(tree)) = self.files.get(&id) else { return self.open_files(id, true) };
        if tree.picked_count() == 0 {
            self.say(Tone::Warning, "Nothing picked. Press space to pick a file.");
            return Vec::new();
        }
        // Explicit paths also let an existing download expand to every file.
        let files = Some(tree.selection());
        self.focus = Focus::List;
        vec![Effect::Get { release: id, files }]
    }

    fn job_action(&mut self, id: TorrentId, action: JobAction) -> Vec<Effect> {
        if self.pending.contains_key(&id) {
            return Vec::new();
        }
        self.pending.insert(id, action);
        vec![Effect::Job { id, action }]
    }

    fn query_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        use KeyCode::*;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            Enter => return self.run_search(),
            Esc if self.help => {
                self.help = false;
                return Vec::new();
            }
            Esc if !self.items().is_empty() => return self.enter_list(),
            Down | Tab => return self.enter_list(),
            BackTab => return self.cycle(-1),
            _ => {}
        }
        let input = &mut self.query;
        match key.code {
            Esc => *input = Input::default(),
            Char('u') if ctrl => *input = Input::default(),
            Char('a') if ctrl => input.caret = 0,
            Char('e') if ctrl => input.caret = input.text.chars().count(),
            Char('k') if ctrl => input.text = input.text.chars().take(input.caret).collect(),
            Char('w') if ctrl => {
                let before: Vec<char> = input.text.chars().take(input.caret).collect();
                let mut cut = before.len();
                while cut > 0 && before[cut - 1] == ' ' {
                    cut -= 1;
                }
                while cut > 0 && before[cut - 1] != ' ' {
                    cut -= 1;
                }
                let after: String = input.text.chars().skip(input.caret).collect();
                input.text = before[..cut].iter().collect::<String>() + &after;
                input.caret = cut;
            }
            Left => input.caret = input.caret.saturating_sub(1),
            Right => input.caret = (input.caret + 1).min(input.text.chars().count()),
            Home => input.caret = 0,
            End => input.caret = input.text.chars().count(),
            Backspace if input.caret > 0 => {
                input.caret -= 1;
                input.text = remove_char(&input.text, input.caret);
            }
            Delete if input.caret < input.text.chars().count() => input.text = remove_char(&input.text, input.caret),
            Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                let mut chars: Vec<char> = input.text.chars().collect();
                chars.insert(input.caret, c);
                input.text = chars.into_iter().collect();
                input.caret += 1;
            }
            _ => {}
        }
        Vec::new()
    }

    fn enter_list(&mut self) -> Vec<Effect> {
        let items = self.items();
        if let Some(cursor) = self.cursor.filter(|c| items.contains(c)) {
            self.focus = Focus::List;
            return match cursor {
                Item::Download(id) => self.refresh_files(id, true),
                Item::Result(_) => Vec::new(),
            };
        }
        let first = self.results().first().map(|r| Item::Result(r.id)).or(items.first().copied());
        match first {
            Some(item) => self.select(item),
            None => Vec::new(),
        }
    }

    fn run_search(&mut self) -> Vec<Effect> {
        let text = sanitize(self.query.text.trim());
        if text.is_empty() {
            return Vec::new();
        }
        self.search_for(text)
    }

    fn search_for(&mut self, text: String) -> Vec<Effect> {
        let id = self.next_search;
        self.next_search += 1;
        self.search = Some(Search {
            id,
            text: text.clone(),
            started: self.now,
            releases: Vec::new(),
            sources: Vec::new(),
            elapsed_ms: 0,
            phase: SearchPhase::Running { pending: Vec::new() },
            ranked_for: None,
        });
        self.query = Input { caret: text.chars().count(), text: text.clone() };
        self.files.clear();
        self.sort = crate::state::Sort::BestMatch;
        self.focus = Focus::List;
        self.follow_top = true;
        if matches!(self.cursor, Some(Item::Result(_))) {
            self.cursor = None;
        }
        let query = SearchQuery { limit: RESULT_LIMIT, ..SearchQuery::new(text) };
        vec![Effect::Search { search: id, query }]
    }

    fn merge(&mut self, id: u64, result: SearchResult, phase: SearchPhase) {
        let Some(search) = self.search.as_mut().filter(|s| s.id == id) else { return };
        search.ranked_for = ranked_for(&result);
        search.releases = result.releases;
        search.sources = result.sources;
        search.elapsed_ms = result.elapsed_ms;
        search.phase = phase;
        let profile = result.query.profile;
        self.preferences = Some(profile.preferences);
        self.machine = profile.detected.or(self.machine.take());
        if self.follow_top
            && self.focus != Focus::Query
            && let Some(first) = self.results().first().map(|r| Item::Result(r.id))
            && self.cursor != Some(first)
        {
            self.cursor = Some(first);
            self.collapsed = false;
        }
    }

    fn watch(&mut self, event: Event) -> Vec<Effect> {
        match event {
            Event::Snapshot { jobs } => {
                self.engine = Engine::Ready;
                for job in &jobs {
                    self.note_arrival(job);
                }
                self.downloads = jobs;
                self.resort();
                self.pending.retain(|id, _| self.downloads.iter().any(|j| j.id == *id));
                if !self.touched
                    && self.focus == Focus::Query
                    && self.cursor.is_none()
                    && let Some(first) = self.downloads.first().map(|j| j.id)
                {
                    let mut effects = self.select(Item::Download(first));
                    effects.push(Effect::FreeSpace);
                    return effects;
                }
                vec![Effect::FreeSpace]
            }
            Event::Job { job } => {
                let id = job.id;
                self.note_arrival(&job);
                match self.downloads.iter_mut().find(|j| j.id == id) {
                    Some(existing) => *existing = *job,
                    None => {
                        self.downloads.push(*job);
                        self.resort();
                    }
                }
                let mut effects = Vec::new();
                if self.cursor == Some(Item::Download(id)) && !self.collapsed {
                    effects.extend(self.refresh_files(id, false));
                }
                let stale = self.free_space_at.is_none_or(|at| self.now.duration_since(at).as_secs() >= 5);
                if stale {
                    self.free_space_at = Some(self.now);
                    effects.push(Effect::FreeSpace);
                }
                effects
            }
            Event::Removed { id } => {
                let Ok(id) = id.parse::<TorrentId>() else { return Vec::new() };
                self.forget(id);
                Vec::new()
            }
            Event::Heartbeat { .. } | Event::SearchUpdate { .. } => Vec::new(),
        }
    }

    fn forget(&mut self, id: TorrentId) {
        if self.cursor == Some(Item::Download(id)) {
            let items = self.items();
            let at = items.iter().position(|i| *i == Item::Download(id)).unwrap_or(0);
            let neighbor = items.get(at + 1).or(at.checked_sub(1).and_then(|p| items.get(p))).copied();
            self.cursor = neighbor;
            self.collapsed = false;
        }
        self.downloads.retain(|j| j.id != id);
        self.download_files.remove(&id);
        self.pending.remove(&id);
        self.arrived.remove(&id);
        self.resort();
    }

    fn refresh_files(&mut self, id: TorrentId, just_selected: bool) -> Vec<Effect> {
        let Some(job) = self.job(id) else { return Vec::new() };
        let known = job.progress.size.is_some();
        let changing = matches!(job.status, JobStatus::Downloading | JobStatus::Checking);
        let due = match self.download_files.get(&id) {
            None => true,
            Some(f) => (just_selected || changing) && self.now.duration_since(f.asked).as_millis() >= 1_000,
        };
        if !known || !due {
            return Vec::new();
        }
        let entry = self.download_files.entry(id).or_insert_with(|| DownloadFiles::new(self.now));
        entry.asked = self.now;
        vec![Effect::Files(Item::Download(id))]
    }

    fn files_arrived(&mut self, item: Item, result: Result<FilesReply, Error>) -> Vec<Effect> {
        match (item, result) {
            (Item::Download(id), Ok(reply)) => {
                if let Some(entry) = self.download_files.get_mut(&id) {
                    entry.files = reply.files;
                }
                Vec::new()
            }
            (Item::Download(_), Err(_)) => Vec::new(),
            (Item::Result(id), _) if matches!(self.files.get(&id), Some(Files::Ready(_))) => Vec::new(),
            (Item::Result(id), result) => {
                let then_download = matches!(self.files.get(&id), Some(Files::Loading { then_download: true, .. }));
                match result {
                    Ok(reply) => {
                        let name = reply.name.clone().or_else(|| self.release(id).map(|r| r.name.clone()));
                        let tree = Tree::new(&name.unwrap_or_default(), reply.files);
                        self.files.insert(id, Files::Ready(tree));
                        if then_download {
                            return self.download(id);
                        }
                    }
                    Err(error) => {
                        if then_download {
                            self.say(Tone::Error, format!("Download failed: {}", error.message));
                        }
                        self.files.insert(id, Files::Failed(error));
                    }
                }
                Vec::new()
            }
        }
    }

    fn started(&mut self, release: ReleaseId, result: Result<Box<GetReply>, Error>) -> Vec<Effect> {
        let titled = self.release(release).map(|r| r.name.clone());
        match result {
            Ok(reply) => {
                let name =
                    titled.or_else(|| reply.job.name.clone()).unwrap_or_else(|| release.to_ref()[..6].to_string());
                let text = match reply.outcome {
                    GetOutcome::Existing => "Already in downloads. Press d to view it.".to_string(),
                    GetOutcome::Updated => format!("Changed the files of {name}"),
                    _ => format!("Started {name}"),
                };
                self.say(Tone::Success, text);
                let job = reply.job;
                self.note_arrival(&job);
                match self.downloads.iter_mut().find(|j| j.id == job.id) {
                    Some(existing) => *existing = job,
                    None => {
                        self.downloads.push(job);
                        self.resort();
                    }
                }
                vec![Effect::FreeSpace]
            }
            Err(error) => {
                self.say(Tone::Error, format!("Download failed: {}", error.message));
                Vec::new()
            }
        }
    }

    fn changed(&mut self, id: TorrentId, action: JobAction, result: Result<Vec<Change>, Error>) -> Vec<Effect> {
        self.pending.remove(&id);
        let name = self.job(id).and_then(|j| j.name.clone()).unwrap_or_else(|| id.to_hex()[..6].to_string());
        match result {
            Ok(_) => {
                let text = match action {
                    JobAction::Pause => format!("Paused {name}"),
                    JobAction::Resume => format!("Resumed {name}"),
                    JobAction::Verify => format!("Verifying {name}"),
                    JobAction::Remove => {
                        let dir = self.job(id).map(|j| home_path(&j.dir, self.home.as_deref()));
                        self.forget(id);
                        match dir {
                            Some(dir) => format!("Removed from the list. Files stay in {dir}."),
                            None => "Removed from the list. Files stay on disk.".into(),
                        }
                    }
                };
                self.say(Tone::Success, text);
            }
            Err(error) => {
                let verb = match action {
                    JobAction::Pause => "Pause",
                    JobAction::Resume => "Resume",
                    JobAction::Verify => "Verify",
                    JobAction::Remove => "Remove",
                };
                self.say(Tone::Error, format!("{verb} failed: {}", error.message));
            }
        }
        Vec::new()
    }

    fn confirm_key(&mut self, id: TorrentId, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Enter | KeyCode::Char('x' | 'y') => {
                self.confirm = None;
                self.job_action(id, JobAction::Remove)
            }
            KeyCode::Esc | KeyCode::Char('n' | 'q') => {
                self.confirm = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

fn remove_char(text: &str, at: usize) -> String {
    text.chars().enumerate().filter(|(i, _)| *i != at).map(|(_, c)| c).collect()
}

fn ranked_for(result: &SearchResult) -> Option<RankedFor> {
    let profile = &result.query.profile;
    if result.is_video() {
        Some(RankedFor { profile: profile.describe(), overrides: profile.overrides() })
    } else if result.releases.iter().any(|r| !Arch::named_in(&r.name).is_empty()) {
        Some(RankedFor { profile: profile.describe_arch(), overrides: None })
    } else {
        None
    }
}
