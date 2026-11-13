use ratatui::layout::Rect;
use torrential_core::{Job, JobStatus, ReleaseId, TorrentId};

use crate::state::{Engine, Files, Focus, Item, ROWS, SearchPhase, State, label};

pub const MIN_SIZE: (u16, u16) = (30, 10);
const WIDE: u16 = 60;
const MAX_CONTENT: u16 = 100;
const CONTEXT: u16 = 2;

pub const MASTHEAD: u16 = 0;
pub const SEARCH: u16 = 2;
const LIST_TOP: u16 = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Indent {
    pub margin: Option<u16>,
    pub mark: u16,
    pub content: u16,
    pub detail: u16,
}

const WITH_MARGIN: Indent = Indent { margin: Some(10), mark: 12, content: 14, detail: 16 };
const NARROW: Indent = Indent { margin: None, mark: 0, content: 2, detail: 4 };

pub struct Screen {
    pub right: u16,
    pub list: Rect,
    pub footer: u16,
    pub at: Indent,
}

impl Indent {
    pub fn narrow(self) -> bool {
        self.margin.is_none()
    }
}

impl Screen {
    pub fn narrow(&self) -> bool {
        self.at.narrow()
    }
}

pub fn screen(size: (u16, u16)) -> Option<Screen> {
    let (w, h) = size;
    if w < MIN_SIZE.0 || h < MIN_SIZE.1 {
        return None;
    }
    let (at, right) =
        if w < WIDE { (NARROW, w - 1) } else { (WITH_MARGIN, (w - 2).min(WITH_MARGIN.content + MAX_CONTENT)) };
    let list = Rect::new(0, LIST_TOP, right, h - LIST_TOP - 2);
    Some(Screen { right, list, footer: h - 1, at })
}

fn narrow(state: &State) -> bool {
    screen(state.size).is_some_and(|s| s.narrow())
}

pub fn list_height(state: &State) -> u16 {
    screen(state.size).map_or(0, |s| s.list.height)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Block {
    Label(&'static str),
    Download(TorrentId),
    DownloadDetail(TorrentId),
    Folded(&'static str),
    Gap,
    Ranking,
    Heads,
    Result(ReleaseId),
    ResultDetail(ReleaseId),
    Empty,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placed {
    pub block: Block,
    pub top: u16,
    pub height: u16,
}

pub fn expanded(state: &State, item: Item) -> bool {
    state.cursor == Some(item) && !state.collapsed && state.focus != Focus::Query && state.ranking.is_none()
}

pub fn blocks(state: &State, viewport: u16) -> Vec<Placed> {
    let mut out = Vec::new();
    let mut top = 0;
    let mut push = |block, height| {
        out.push(Placed { block, top, height });
        top += height;
    };
    if !state.downloads.is_empty() {
        let mut last = None;
        for job in state.download_rows() {
            let word = label(&job.status);
            if narrow(state) && last != Some(word) {
                push(Block::Label(word), 1);
            }
            last = Some(word);
            push(Block::Download(job.id), 1);
            if expanded(state, Item::Download(job.id)) || state.confirm == Some(job.id) {
                push(Block::DownloadDetail(job.id), download_detail_height(state, job, viewport));
            }
        }
        if state.folded().is_some() {
            for fold in ["paused", "done"] {
                if state.downloads.iter().any(|j| label(&j.status) == fold) {
                    push(Block::Folded(fold), 1);
                }
            }
        }
        push(Block::Gap, 1);
    }
    if state.ranking.is_some() {
        push(Block::Ranking, 1 + ROWS.len() as u16);
        push(Block::Gap, 1);
    }
    let results = state.results();
    if results.is_empty() {
        if empty_message(state) {
            push(Block::Empty, 2);
        }
        return out;
    }
    push(Block::Heads, 1);
    for release in results {
        push(Block::Result(release.id), 1);
        if expanded(state, Item::Result(release.id)) {
            push(Block::ResultDetail(release.id), result_detail_height(state, release.id, viewport));
        }
    }
    out
}

fn empty_message(state: &State) -> bool {
    match &state.search {
        None => matches!(state.engine, Engine::Unreachable(_)),
        Some(search) => state.filtering() || !matches!(search.phase, SearchPhase::Running { .. }),
    }
}

pub fn in_files(state: &State, id: TorrentId) -> bool {
    state.focus == Focus::Files && state.cursor == Some(Item::Download(id))
}

pub fn download_file_rows(state: &State, job: &Job, viewport: u16) -> (usize, bool) {
    let n = state.download_files.get(&job.id).map_or(0, |f| f.picked().len());
    if n <= 1 {
        return (0, false);
    }
    if in_files(state, job.id) {
        let fixed = 5 + error_lines(job) + confirm_lines(state, job);
        return (n.min(usize::from(viewport.saturating_sub(fixed).max(1))), false);
    }
    let cap = match viewport {
        24.. => 8,
        16.. => 5,
        _ => 3,
    };
    (n.min(cap), n > cap)
}

fn confirm_lines(state: &State, job: &Job) -> u16 {
    match state.confirm == Some(job.id) {
        false => 0,
        true if narrow(state) => 2,
        true => 1,
    }
}

fn error_lines(job: &Job) -> u16 {
    match &job.status {
        JobStatus::Error { error } => 1 + u16::from(error.hint.is_some()),
        _ => 0,
    }
}

fn download_detail_height(state: &State, job: &Job, viewport: u16) -> u16 {
    let (files, more) = download_file_rows(state, job, viewport);
    2 + error_lines(job) + confirm_lines(state, job) + files as u16 + u16::from(more)
}

pub fn tree_rows(nodes: usize, viewport: u16) -> usize {
    nodes.min(usize::from(viewport.saturating_sub(5).max(1)))
}

fn result_detail_height(state: &State, id: ReleaseId, viewport: u16) -> u16 {
    if state.focus != Focus::Files {
        return if narrow(state) { 2 } else { 1 };
    }
    match state.files.get(&id) {
        Some(Files::Ready(tree)) => tree_rows(tree.visible().len(), viewport) as u16 + 2,
        _ => 2,
    }
}

pub fn span(placed: &[Placed], item: Item) -> Option<(u16, u16)> {
    let row = |b: &Placed| match (b.block, item) {
        (Block::Download(id), Item::Download(want)) => id == want,
        (Block::Result(id), Item::Result(want)) => id == want,
        _ => false,
    };
    let at = placed.iter().position(row)?;
    let detail = placed.get(at + 1).filter(|b| matches!(b.block, Block::DownloadDetail(_) | Block::ResultDetail(_)));
    Some((placed[at].top, placed[at].height + detail.map_or(0, |d| d.height)))
}

pub fn ranking_span(placed: &[Placed]) -> Option<(u16, u16)> {
    placed.iter().find(|b| b.block == Block::Ranking).map(|b| (b.top, b.height))
}

pub fn total(placed: &[Placed]) -> u16 {
    placed.last().map_or(0, |b| b.top + b.height)
}

/// The scroll that keeps `lines`, the cursor's top row and height, in view with some context
/// around them, in a list `total` rows long.
pub fn settle(total: u16, lines: Option<(u16, u16)>, scroll: i32, viewport: u16) -> u16 {
    let viewport = i32::from(viewport);
    let max = (i32::from(total) - viewport).max(0);
    let mut scroll = scroll.clamp(0, max);
    if let Some((top, height)) = lines {
        let context = i32::from(CONTEXT).min((viewport - i32::from(height)).max(0) / 2);
        let (top, bottom) = (i32::from(top), i32::from(top + height));
        if bottom + context > scroll + viewport {
            scroll = bottom + context - viewport;
        }
        if top - context < scroll {
            scroll = top - context;
        }
    }
    scroll.clamp(0, max) as u16
}
