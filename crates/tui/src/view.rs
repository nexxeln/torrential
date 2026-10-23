use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use torrential_core::units::{count, format_duration};
use torrential_core::{
    Eighths, ErrorCode, Flag, Job, JobStatus, Machine, Preferences, Profile, Quality, Release, ReleaseId, Resolution,
    Slice, SourceOutcome, TorrentFile,
};

use crate::format::{
    self, age, ellipsize, first_fit, home_path, middle, percent, rate, seeds, shortenings, size, width,
};
use crate::layout::{self, Block, Indent, MASTHEAD, SEARCH, Screen};
use crate::state::{
    Engine, Files, Focus, Item, ROWS, RankedFor, Ranking, Row, SearchPhase, Sort, State, Tone, Upgrade, label,
};
use crate::theme::*;
use crate::tree::{Check, NodeKind, Tree};

fn fg(c: Color) -> Style {
    Style::new().fg(c)
}

fn bold(c: Color) -> Style {
    fg(c).add_modifier(Modifier::BOLD)
}

fn put(buf: &mut Buffer, x: u16, y: u16, max: u16, text: &str, style: Style) -> u16 {
    let area = buf.area;
    if max == 0 || y < area.y || y >= area.bottom() || x >= area.right() {
        return 0;
    }
    let max = max.min(area.right() - x);
    let text = ellipsize(text, max as usize);
    buf.set_stringn(x, y, &text, max as usize, style);
    width(&text) as u16
}

fn put_right(buf: &mut Buffer, right: u16, y: u16, text: &str, style: Style) -> u16 {
    let x = right.saturating_sub(width(text) as u16);
    put(buf, x, y, right - x, text, style);
    x
}

fn spans(buf: &mut Buffer, x: u16, y: u16, max: u16, parts: &[(impl AsRef<str>, Style)]) -> u16 {
    let mut used = 0;
    for (text, style) in parts {
        used += put(buf, x + used, y, max.saturating_sub(used), text.as_ref(), *style);
    }
    used
}

fn spans_width(parts: &[(impl AsRef<str>, Style)]) -> u16 {
    parts.iter().map(|(t, _)| width(t.as_ref()) as u16).sum()
}

fn spans_right(buf: &mut Buffer, right: u16, y: u16, parts: &[(impl AsRef<str>, Style)]) -> u16 {
    let w = spans_width(parts);
    spans(buf, right.saturating_sub(w), y, w, parts);
    right.saturating_sub(w)
}

fn paint(buf: &mut Buffer) {
    for cell in &mut buf.content {
        cell.set_symbol(" ").set_bg(PAPER).set_fg(TEXT);
    }
}

fn margin(buf: &mut Buffer, at: Indent, y: u16, text: &str, style: Style) {
    if let Some(edge) = at.margin {
        put_right(buf, edge + 1, y, text, style);
    }
}

fn mark(buf: &mut Buffer, at: Indent, y: u16) {
    put(buf, at.mark, y, 1, "●", fg(ACTION));
}

fn arrow(buf: &mut Buffer, at: Indent, y: u16, glyph: &str) {
    if buf[(at.mark, y)].symbol() == "│" {
        put(buf, at.mark, y, 1, glyph, fg(PENCIL));
    }
}

fn rule(buf: &mut Buffer, at: Indent, top: u16, bottom: u16) {
    for y in top..bottom {
        put(buf, at.mark, y, 1, "│", fg(RULE));
    }
}

enum Meter {
    Share { verified: u64, size: Option<u64>, getting: bool },
    Pieces(Vec<Eighths>),
}

impl Meter {
    fn job(state: &State, job: &Job) -> Self {
        let p = &job.progress;
        let getting = state.getting(job);
        let wanted: Vec<Eighths> = match p.piece_map {
            Some(map) if getting => map
                .slices()
                .iter()
                .filter_map(|slice| match slice {
                    Slice::Wanted(eighths) => Some(*eighths),
                    Slice::Unwanted => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        if wanted.is_empty() {
            Self::Share { verified: p.verified, size: p.size, getting }
        } else {
            Self::Pieces(wanted)
        }
    }

    fn file(state: &State, job: &Job, file: &TorrentFile) -> Self {
        Self::Share { verified: file.verified, size: Some(file.size), getting: state.getting(job) }
    }
}

fn progress_mark(buf: &mut Buffer, x: u16, y: u16, cells: u16, meter: Meter) {
    match meter {
        Meter::Share { verified, size, getting } => {
            let ink = if getting { ACTION } else { PENCIL };
            let fraction = match size {
                Some(0) => 1.0,
                Some(size) => verified as f64 / size as f64,
                None => 0.0,
            };
            let halves = (fraction.clamp(0.0, 1.0) * f64::from(cells) * 2.0).floor() as u16;
            for i in 0..cells {
                let (glyph, color) = match halves.saturating_sub(i * 2) {
                    0 => ("─", RULE),
                    1 => ("╸", ink),
                    _ => ("━", ink),
                };
                put(buf, x + i, y, 1, glyph, fg(color));
            }
        }
        Meter::Pieces(wanted) => {
            const BARS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
            let span = |i: u16| usize::from(i) * wanted.len() / usize::from(cells);
            for i in 0..cells {
                let run = &wanted[span(i)..span(i + 1).max(span(i) + 1)];
                let got = run.iter().map(|eighths| u32::from(eighths.get())).sum();
                let (glyph, color) = match Slice::of(got, 8 * run.len() as u32) {
                    Slice::Wanted(eighths) if eighths.get() > 0 => (BARS[usize::from(eighths.get()) - 1], ACTION),
                    _ => ("▁", RULE),
                };
                put(buf, x + i, y, 1, glyph, fg(color));
            }
        }
    }
}

fn hints_width(pairs: &[(&str, &str)]) -> u16 {
    let cells = pairs.iter().map(|(k, v)| width(k) + 1 + width(v)).sum::<usize>() + pairs.len().saturating_sub(1) * 3;
    cells as u16
}

fn hints(buf: &mut Buffer, right: u16, y: u16, room: u16, pairs: &[(&str, &str)], kept: &[(&str, &str)]) {
    let mut pairs = pairs.to_vec();
    while hints_width(&pairs) > room
        && let Some(last) = pairs.iter().rposition(|pair| !kept.contains(pair))
    {
        pairs.remove(last);
    }
    let mut x = right.saturating_sub(hints_width(&pairs));
    for (i, (key, verb)) in pairs.iter().enumerate() {
        if i > 0 {
            x += 3;
        }
        x += put(buf, x, y, right.saturating_sub(x), key, fg(TEXT));
        x += put(buf, x, y, right.saturating_sub(x), &format!(" {verb}"), fg(PENCIL));
    }
}

pub fn render(buf: &mut Buffer, state: &State, mode: ColorMode) -> Option<Position> {
    paint(buf);
    let size = (buf.area.width, buf.area.height);
    let Some(screen) = layout::screen(size) else {
        let (w, h) = layout::MIN_SIZE;
        let one = format!("Make the window at least {w}×{h}.");
        let lines = match width(&one) as u16 + 2 <= size.0 {
            true => vec![one],
            false => vec!["Make the window".to_string(), format!("at least {w}×{h}.")],
        };
        for (y, line) in (0..).zip(lines) {
            put(buf, 1, y, size.0.saturating_sub(2), &line, fg(INK));
        }
        degrade(buf, mode);
        return None;
    };
    masthead(buf, state, &screen);
    let caret = search(buf, state, &screen);
    rule(buf, screen.at, SEARCH + 1, screen.list.y);
    if state.help {
        help(buf, &screen);
    } else {
        list(buf, state, &screen);
    }
    footer(buf, state, &screen);
    degrade(buf, mode);
    caret
}

fn masthead(buf: &mut Buffer, state: &State, screen: &Screen) {
    let at = screen.at;
    let used = match at.margin {
        Some(_) => {
            margin(buf, at, MASTHEAD, "torrential", bold(INK));
            0
        }
        None => at.content + put(buf, at.content, MASTHEAD, screen.right - at.content, "torrential", bold(INK)) + 3,
    };
    let room = screen.right.saturating_sub(used);
    let total = rate(state.rate());
    let free = state.free_space.filter(|_| short_of_space(state)).map(size);
    let free = [free.as_ref().map(|f| format!("low on space · {f} free")), free.map(|f| format!("{f} free"))];
    let mut parts: Vec<(&str, Style)> = Vec::new();
    match &state.engine {
        Engine::Starting => {
            let text = if room >= 20 { "starting the engine" } else { "starting" };
            parts.extend([(text, fg(PENCIL)), (state.spinner(), fg(PENCIL))]);
        }
        Engine::Unreachable(_) => {
            parts.push((if room >= 18 { "engine unreachable" } else { "unreachable" }, fg(ERROR)));
        }
        Engine::Ready => {
            if state.rate() > 0 {
                parts.extend([("↓ ", fg(ACTION)), (total.as_str(), fg(TEXT))]);
            }
            let gap = if parts.is_empty() { 0 } else { 3 };
            let fits = |f: &&String| spans_width(&parts) + gap + width(f) as u16 <= room;
            if let Some(free) = free.iter().flatten().find(fits) {
                if gap > 0 {
                    parts.push(("   ", fg(PENCIL)));
                }
                parts.push((free, fg(ERROR)));
            }
        }
    }
    spans_right(buf, screen.right, MASTHEAD, &parts);
}

fn short_of_space(state: &State) -> bool {
    let needed: u64 = state
        .downloads
        .iter()
        .filter(|j| crate::state::group(&j.status) == 0)
        .filter_map(|j| j.progress.size.map(|s| s.saturating_sub(j.progress.verified)))
        .sum();
    state.free_space.is_some_and(|free| free < needed.max(1 << 30))
}

fn search(buf: &mut Buffer, state: &State, screen: &Screen) -> Option<Position> {
    let at = screen.at;
    let focused = state.focus == Focus::Query;
    margin(buf, at, SEARCH, "search", fg(PENCIL));
    if focused {
        mark(buf, at, SEARCH);
    } else {
        rule(buf, at, SEARCH, SEARCH + 1);
    }
    let text = &state.query.text;
    let hint = if focused { "type a name, then press enter" } else { "press / to search" };
    let left = width(if text.is_empty() { hint } else { text }) as u16;
    let room = screen.right - at.content;
    let [long, short] = [false, true].map(|short| search_status(state, short));
    let status = if left + 3 + spans_width(&long) <= room { long } else { short };
    let used = spans_width(&status);
    spans_right(buf, screen.right, SEARCH, &status);
    let (x, room) = (at.content, if used == 0 { room } else { room.saturating_sub(used + 3) });
    if text.is_empty() {
        put(buf, x, SEARCH, room, hint, fg(PENCIL));
        return focused.then_some(Position::new(x, SEARCH));
    }
    if !focused {
        put(buf, x, SEARCH, room, text, fg(INK));
        return None;
    }
    let before: String = text.chars().take(state.query.caret).collect();
    let skip = (width(&before) as u16).saturating_sub(room.saturating_sub(1));
    let shown: String = {
        let mut cells = 0;
        text.chars()
            .skip_while(|c| {
                let skipping = cells < skip;
                cells += width(c.encode_utf8(&mut [0; 4])) as u16;
                skipping
            })
            .collect()
    };
    put(buf, x, SEARCH, room, &shown, fg(INK));
    Some(Position::new(x + width(&before) as u16 - skip, SEARCH))
}

fn reason(outcome: &SourceOutcome) -> String {
    match outcome {
        SourceOutcome::Ok { .. } | SourceOutcome::Cached { .. } => "ok".into(),
        SourceOutcome::Timeout => "timed out".into(),
        SourceOutcome::Http { status } => format!("answered {status}"),
        SourceOutcome::Connect { .. } => "did not connect".into(),
        SourceOutcome::BadResponse { .. } => "sent a bad answer".into(),
        SourceOutcome::Irrelevant => "had nothing relevant".into(),
    }
}

fn and_list(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn search_status(state: &State, short: bool) -> Vec<(String, Style)> {
    let Some(search) = &state.search else { return Vec::new() };
    if state.filtering() {
        let (shown, all) = (state.results().len(), search.releases.len());
        let text =
            if short { format!("{shown} of {all}") } else { format!("{shown} of {all} match · enter searches") };
        return vec![(text, fg(PENCIL))];
    }
    match &search.phase {
        SearchPhase::Running { pending } => {
            let (answered, asked) = (search.sources.len(), search.sources.len() + pending.len());
            let text = match asked {
                0 => "searching".to_string(),
                _ if short => format!("{answered} of {asked}"),
                _ => format!("searching · {answered} of {}", count(asked, "source")),
            };
            vec![(text, fg(PENCIL)), (state.spinner().into(), fg(PENCIL))]
        }
        SearchPhase::Done => {
            let found = search.releases.len();
            let secs = f64::from(search.elapsed_ms) / 1000.0;
            let count = if short { format!("{found} found") } else { format!("{found} found in {secs:.1}s") };
            let mut out = vec![(count, fg(PENCIL))];
            let failed: Vec<&str> =
                search.sources.iter().filter(|s| !s.outcome.is_ok()).map(|s| s.source.as_str()).collect();
            if !failed.is_empty() {
                let text =
                    if short { format!("{} failed", failed.len()) } else { format!("{} failed", and_list(&failed)) };
                out.extend([(" · ".into(), fg(PENCIL)), (text, fg(ERROR))]);
            }
            out
        }
        SearchPhase::Failed(_) => vec![("search failed".into(), fg(ERROR))],
    }
}

struct Grid {
    at: Indent,
    rule: u16,
    rule_cells: u16,
    percent: u16,
    tail: Option<u16>,
    right: u16,
}

fn download_grid(screen: &Screen) -> Grid {
    let (at, right) = (screen.at, screen.right);
    if at.narrow() {
        let rule_cells = if right - at.content >= 40 { 10 } else { 6 };
        return Grid { at, rule: right - 5 - rule_cells, rule_cells, percent: right, tail: None, right };
    }
    let rule_cells = match right - at.content {
        80.. => 16,
        70.. => 12,
        _ => 8,
    };
    let tail = right - 12;
    let percent = tail - 2;
    Grid { at, rule: percent - 6 - rule_cells, rule_cells, percent, tail: Some(tail), right }
}

struct Columns {
    at: Indent,
    names: u16,
    flag: Option<u16>,
    quality: Option<u16>,
    size: u16,
    seeds: Option<u16>,
    age: Option<u16>,
    right: u16,
}

fn result_columns(screen: &Screen, with_quality: bool) -> Columns {
    let (at, right) = (screen.at, screen.right);
    if at.narrow() {
        let quality = (with_quality && right - 15 >= at.content + 17).then_some(right - 15);
        let names = quality.map_or(right - 10, |x| x - 1);
        return Columns { at, names, flag: None, quality, size: right, seeds: None, age: None, right };
    }
    let seeds = right - 6;
    let size = seeds - 7;
    let quality = size - 15;
    let names = (quality - 10).saturating_sub(at.content + 2);
    let quality = (with_quality && names >= 20).then_some(quality);
    let flag = quality.unwrap_or(size - 8) - 10;
    Columns { at, names: flag - 2, flag: Some(flag), quality, size, seeds: Some(seeds), age: Some(right), right }
}

fn before_size(c: &Columns) -> u16 {
    c.size - 10
}

fn list(buf: &mut Buffer, state: &State, screen: &Screen) {
    let at = screen.at;
    let area = screen.list;
    let placed = layout::blocks(state, area.height);
    let content = layout::total(&placed);
    let total = content.max(area.height);
    let mut sheet = Buffer::empty(Rect::new(0, 0, area.width, total));
    paint(&mut sheet);
    rule(&mut sheet, at, 0, total);
    let mut group: Vec<Option<&'static str>> = vec![None; usize::from(total)];
    let mut last: Option<&'static str> = None;
    let right = screen.right;
    let grid = download_grid(screen);
    let columns = result_columns(screen, state.results().iter().any(|r| quality_word(r.quality()).is_some()));
    for p in &placed {
        let y = p.top;
        match p.block {
            Block::Label(word) => {
                let style = if word == "error" { fg(ERROR) } else { fg(PENCIL) };
                put(&mut sheet, at.content, y, right - at.content, word, style);
                last = Some(word);
            }
            Block::Download(id) => {
                let Some(job) = state.job(id) else { continue };
                let word = if state.confirm == Some(id) { "remove?" } else { label(&job.status) };
                if last != Some(word) {
                    let style = if matches!(word, "remove?" | "error") { fg(ERROR) } else { fg(PENCIL) };
                    margin(&mut sheet, at, y, word, style);
                }
                last = Some(word);
                download_row(&mut sheet, state, job, &grid, y);
            }
            Block::DownloadDetail(id) => {
                if layout::in_files(state, id) {
                    last = Some("files");
                }
                if let Some(job) = state.job(id) {
                    download_detail(&mut sheet, state, job, &grid, (y, area.height));
                }
            }
            Block::Folded(word) => {
                last = Some(word);
                let n = state.downloads.iter().filter(|j| label(&j.status) == word).count();
                let text = match at.margin {
                    Some(_) => {
                        margin(&mut sheet, at, y, word, fg(PENCIL));
                        count(n, "download")
                    }
                    None => format!("{word} · {}", count(n, "download")),
                };
                put(&mut sheet, at.content, y, right - at.content, &text, fg(PENCIL));
            }
            Block::Gap => last = None,
            Block::Ranking => {
                last = Some("ranking");
                if let Some(ranking) = &state.ranking {
                    ranking_rows(&mut sheet, state, ranking, screen, y);
                }
            }
            Block::Heads => {
                last = Some("found");
                heads(&mut sheet, state, &columns, y);
            }
            Block::Result(id) => {
                if let Some(release) = state.release(id) {
                    result_row(&mut sheet, state, release, &columns, y);
                }
            }
            Block::ResultDetail(id) => {
                if state.focus == Focus::Files {
                    last = Some("files");
                }
                result_detail(&mut sheet, state, id, &columns, y, p.height);
            }
            Block::Empty => empty(&mut sheet, state, screen, y),
        }
        for row in y..y + p.height {
            group[usize::from(row)] = last;
        }
    }
    let scroll = state.scroll.min(total - area.height);
    for row in 0..area.height {
        for x in 0..area.width {
            buf[(x, area.y + row)] = sheet[(x, scroll + row)].clone();
        }
    }
    let blank_margin = at.margin.is_some_and(|edge| (0..=edge).all(|x| buf[(x, area.y)].symbol() == " "));
    if scroll > 0
        && blank_margin
        && let Some(word) = group[usize::from(scroll)]
    {
        margin(buf, at, area.y, word, fg(PENCIL));
    }
    if scroll > 0 {
        arrow(buf, at, area.y, "↑");
    }
    if scroll + area.height < content {
        arrow(buf, at, area.bottom() - 1, "↓");
    }
}

fn selected(state: &State, item: Item) -> bool {
    state.cursor == Some(item) && state.focus != Focus::Query && state.ranking.is_none()
}

fn ranking_rows(buf: &mut Buffer, state: &State, ranking: &Ranking, screen: &Screen, y: u16) {
    let (at, right) = (screen.at, screen.right);
    margin(buf, at, y, "ranking", fg(PENCIL));
    let room = right - at.content;
    let lead = if at.narrow() { "ranking for " } else { "for " };
    let profile = Profile { detected: state.machine.clone(), preferences: ranking.picks, room: None };
    let heads = first_fit(shortenings(&profile.describe()), usize::from(room) - lead.len());
    put(buf, at.content, y, room, &heads.map(|heads| format!("{lead}{heads}")).unwrap_or_default(), fg(PENCIL));
    for (row, y) in ROWS.into_iter().zip(y + 1..) {
        let current = row == ranking.row;
        if current {
            mark(buf, at, y);
        }
        put(buf, at.content, y, room, row.name(), if current { bold(INK) } else { fg(TEXT) });
        let picked = ranking.picked(row);
        let mut x = at.content + 10;
        for (k, choice) in ranking.choices(row).into_iter().enumerate() {
            let (text, style) = match row.value(choice) {
                value if k == picked => (format!("[{value}]"), bold(INK)),
                value => (format!(" {value} "), fg(PENCIL)),
            };
            x += put(buf, x, y, right.saturating_sub(x), &text, style);
        }
        if let Some(note) = auto_note(row, ranking, state.machine.as_ref())
            && x + 2 + width(&note) as u16 <= right
        {
            put_right(buf, right, y, &note, fg(PENCIL));
        }
    }
}

fn auto_note(row: Row, ranking: &Ranking, machine: Option<&Machine>) -> Option<String> {
    match (row, machine) {
        (Row::MaxSize, _) => ranking.picks.max_size.map(|_| "refuses larger downloads".into()),
        (_, None) => Some("not detected yet".into()),
        (Row::Screen, Some(machine)) => Some(match machine.display {
            Some(display) => format!("detected {}", display.target().as_str()),
            None => "no screen detected".into(),
        }),
        (Row::Playback, Some(machine)) => {
            let preferences = Preferences { playback: None, ..ranking.picks };
            let auto = Profile { detected: Some(machine.clone()), preferences, room: None };
            Some(auto.playback().map_or_else(|| "plays anything".into(), |p| format!("detected {}", p.value.as_str())))
        }
    }
}

fn download_row(buf: &mut Buffer, state: &State, job: &Job, g: &Grid, y: u16) {
    let content = g.at.content;
    let is_selected = selected(state, Item::Download(job.id));
    if is_selected && state.focus == Focus::List {
        mark(buf, g.at, y);
    }
    let done = matches!(job.status, JobStatus::Done { .. });
    let ink = match (is_selected, done) {
        (true, _) => bold(INK),
        (false, true) => fg(PENCIL),
        (false, false) => fg(TEXT),
    };
    let name = job.name.clone().unwrap_or_else(|| job.id.to_hex());
    let p = &job.progress;
    if done {
        let total = p.size.map_or("–".into(), size);
        let room = g.tail.unwrap_or(g.right - width(&total) as u16) - 2 - content;
        put(buf, content, y, room, &format::name(&name, room.into()), ink);
        put_right(buf, g.right, y, &total, ink);
        return;
    }
    let why = match (&job.status, g.tail) {
        (JobStatus::Error { error }, None) => Some(failure(error.code)),
        _ => None,
    };
    let bar_end = g.rule + g.rule_cells;
    let room = why.as_ref().map_or(g.rule, |why| g.rule.min(bar_end.saturating_sub(width(why) as u16))) - 2 - content;
    put(buf, content, y, room, &format::name(&name, room.into()), ink);
    let pending =
        state.pending.get(&job.id).map(|action| [(action.progressive(), fg(PENCIL)), (state.spinner(), fg(PENCIL))]);
    if let (Some(pending), None) = (&pending, g.tail) {
        spans(buf, g.rule, y, g.right - g.rule, pending);
        return;
    }
    match &why {
        Some(why) => _ = put_right(buf, bar_end, y, why, fg(ERROR)),
        None => progress_mark(buf, g.rule, y, g.rule_cells, Meter::job(state, job)),
    }
    let pct = p.size.map_or("–".to_string(), |size| percent(p.verified, size));
    put_right(buf, g.percent, y, &pct, ink);
    let Some(tail) = g.tail else { return };
    if let Some(pending) = &pending {
        spans(buf, tail, y, g.right - tail, pending);
        return;
    }
    let style = match (&job.status, is_selected) {
        (JobStatus::Error { .. }, _) => fg(ERROR),
        (_, true) => ink,
        (_, false) => fg(PENCIL),
    };
    put_right(buf, g.right, y, &tail_text(state, job), style);
}

fn failure(code: ErrorCode) -> String {
    match code {
        ErrorCode::StorageError => "disk error",
        ErrorCode::VerificationFailed => "bad data",
        ErrorCode::MetadataTimeout => "no metadata",
        ErrorCode::NetworkUnavailable => "offline",
        ErrorCode::NoSelection => "no files",
        ErrorCode::InvalidTorrent => "bad torrent",
        ErrorCode::PathConflict => "path taken",
        ErrorCode::SandboxNetworkBlocked | ErrorCode::SandboxWriteBlocked => "sandboxed",
        code => code.as_str(),
    }
    .replace('_', " ")
}

fn tail_text(state: &State, job: &Job) -> String {
    let p = &job.progress;
    match &job.status {
        JobStatus::Downloading => match p.eta {
            Some(secs) if p.down_rate >= 1024 => format!("{} left", format_duration(Duration::from_secs(secs))),
            _ if state.stalled(job) => "stalled".into(),
            _ => String::new(),
        },
        JobStatus::Metadata => count(p.peers as usize, "peer"),
        JobStatus::Error { error } => failure(error.code),
        _ => String::new(),
    }
}

fn download_detail(buf: &mut Buffer, state: &State, job: &Job, g: &Grid, (y, viewport): (u16, u16)) {
    let (at, detail) = (g.at, g.at.detail);
    let room = g.right - detail;
    let home = state.home.as_deref();
    let mut row = y;
    if state.confirm == Some(job.id) {
        let text = format!("Its files stay in {}.", home_path(&job.dir, home));
        let keys = [("enter", fg(INK)), (" removes it   ", fg(PENCIL)), ("esc", fg(INK)), (" keeps it", fg(PENCIL))];
        if at.narrow() {
            put(buf, detail, row, room, &text, fg(TEXT));
            row += 1;
            spans(buf, detail, row, room, &keys);
        } else {
            let keys_x = spans_right(buf, g.right, row, &keys);
            put(buf, detail, row, keys_x.saturating_sub(detail + 2), &text, fg(TEXT));
        }
        row += 1;
    }
    let path = job.path.as_ref().unwrap_or(&job.dir);
    put(buf, detail, row, room, &middle(&home_path(path, home), room as usize), fg(PENCIL));
    let p = &job.progress;
    let mut facts = Vec::new();
    match (&job.status, p.size) {
        (JobStatus::Metadata, _) | (_, None) => facts.push("fetching the file list from peers".to_string()),
        (JobStatus::Done { finished_at }, Some(total)) => {
            facts.push(size(total));
            facts.push(format!("finished {} ago", age(*finished_at, state.clock)));
        }
        (_, Some(total)) => facts.push(format!("{} of {}", size(p.verified), size(total))),
    }
    if job.status == JobStatus::Downloading {
        facts.push(rate(p.down_rate));
    }
    if at.narrow() && job.status == JobStatus::Downloading {
        let left = tail_text(state, job);
        if !left.is_empty() {
            facts.push(left);
        }
    }
    if matches!(job.status, JobStatus::Downloading | JobStatus::Metadata) {
        facts.push(count(p.peers as usize, "peer"));
    }
    while facts.len() > 1 && width(&facts.join(" · ")) > usize::from(room) {
        facts.pop();
    }
    put(buf, detail, row + 1, room, &facts.join(" · "), fg(PENCIL));
    row += 2;
    if let JobStatus::Error { error } = &job.status {
        put(buf, detail, row, room, &error.message, fg(ERROR));
        row += 1;
        if let Some(hint) = &error.hint {
            put(buf, detail, row, room, hint, fg(PENCIL));
            row += 1;
        }
    }
    let (shown, more) = layout::download_file_rows(state, job, viewport);
    let Some(files) = state.download_files.get(&job.id) else { return };
    let picked = files.picked();
    let inside = layout::in_files(state, job.id);
    let skip = if inside { files.scroll } else { 0 };
    let first = row;
    for (k, file) in picked.iter().enumerate().skip(skip).take(shown) {
        let here = inside && k == files.cursor;
        if inside && row == first {
            margin(buf, at, row, "files", fg(PENCIL));
        }
        if here {
            mark(buf, at, row);
        }
        let name = file.path.rsplit('/').next().unwrap_or(&file.path);
        let room = g.rule - 2 - detail;
        put(buf, detail, row, room, &middle(name, room as usize), if here { bold(INK) } else { fg(TEXT) });
        progress_mark(buf, g.rule, row, g.rule_cells, Meter::file(state, job, file));
        put_right(buf, g.percent, row, &percent(file.verified, file.size), fg(PENCIL));
        if !at.narrow() {
            put_right(buf, g.right, row, &size(file.size), fg(PENCIL));
        }
        row += 1;
    }
    if skip > 0 {
        arrow(buf, at, first, "↑");
    }
    if inside && skip + shown < picked.len() {
        arrow(buf, at, row - 1, "↓");
    }
    if more {
        let text = format!("and {} · press enter to list them", count(picked.len() - shown, "more file"));
        put(buf, detail, row, room, &text, fg(PENCIL));
    }
}

fn heads(buf: &mut Buffer, state: &State, c: &Columns, y: u16) {
    margin(buf, c.at, y, "found", fg(PENCIL));
    let room = c.size - "size".len() as u16 - 2 - c.at.content;
    let ranked_for = state.search.as_ref().and_then(|s| s.ranked_for.as_ref());
    let saved = if state.ranking.is_some_and(|r| r.picks != r.saved) { " (saved)" } else { "" };
    let fit = usize::from(room).saturating_sub("ranked for ".len() + saved.len());
    let ranked = ranked_for.and_then(|r| first_fit(ranked_lines(r), fit));
    let order = match (state.sort, ranked) {
        (Sort::BestMatch, Some(ranked)) => format!("ranked for {ranked}{saved}"),
        (Sort::BestMatch, None) => "best match first".into(),
        (Sort::Seeds, _) => "most seeds first".into(),
        (Sort::Size, _) => "largest first".into(),
        (Sort::Age, _) => "newest first".into(),
    };
    put(buf, c.at.content, y, room, &order, fg(PENCIL));
    put_right(buf, c.size, y, "size", fg(PENCIL));
    if let Some(x) = c.seeds {
        put_right(buf, x, y, "seeds", fg(PENCIL));
    }
    if let Some(x) = c.age {
        put_right(buf, x, y, "age", fg(PENCIL));
    }
}

fn ranked_lines(ranked: &RankedFor) -> Vec<String> {
    let shorter = shortenings(&ranked.profile);
    match &ranked.overrides {
        None => shorter,
        Some(overrides) => {
            let label = shorter.last().cloned();
            shorter.into_iter().map(|profile| format!("{profile} · overrides: {overrides}")).chain(label).collect()
        }
    }
}

fn first_flag(release: &Release) -> Option<Flag> {
    let rank = [Flag::Cam, Flag::Official, Flag::PublicDomain, Flag::CreativeCommons, Flag::Trusted, Flag::Adult];
    rank.into_iter().find(|f| release.flags.contains(f))
}

fn flag_words(flag: Flag) -> &'static str {
    match flag {
        Flag::Official => "official release",
        Flag::Trusted => "trusted uploader",
        Flag::PublicDomain => "public domain",
        Flag::CreativeCommons => "creative commons",
        Flag::Cam => "recorded in a cinema",
        Flag::Adult => "adult",
    }
}

fn quality_word(quality: Quality) -> Option<&'static str> {
    quality.resolution.map(Resolution::as_str)
}

fn result_row(buf: &mut Buffer, state: &State, release: &Release, c: &Columns, y: u16) {
    let is_selected = selected(state, Item::Result(release.id));
    if is_selected && state.focus == Focus::List {
        mark(buf, c.at, y);
    }
    let pick = |plain: Color| if is_selected { bold(INK) } else { fg(plain) };
    let room = c.names - c.at.content;
    put(buf, c.at.content, y, room, &format::name(&release.name, room.into()), pick(TEXT));
    if let (Some(x), Some(flag)) = (c.flag, first_flag(release)) {
        let style = match flag {
            Flag::Cam => fg(ERROR),
            Flag::Trusted | Flag::Adult => pick(PENCIL),
            _ => pick(TEXT),
        };
        put(buf, x, y, 8, flag.as_str(), style);
    }
    if let (Some(x), Some(word)) = (c.quality, quality_word(release.quality())) {
        put(buf, x, y, 5, word, pick(TEXT));
    }
    put_right(buf, c.size, y, &release.size.map_or("–".into(), size), pick(TEXT));
    if let Some(x) = c.seeds {
        let seeds_style = match release.seeders {
            Some(0) => fg(ERROR),
            None | Some(1..=9) => pick(PENCIL),
            Some(_) => pick(TEXT),
        };
        let web = release.seeders.is_none() && release.sources.iter().any(|s| s == "archive.org");
        put_right(buf, x, y, &if web { "web".into() } else { seeds(release.seeders) }, seeds_style);
    }
    if let Some(x) = c.age {
        let published = release.published.map_or("–".into(), |p| age(p, state.clock));
        put_right(buf, x, y, &published, pick(PENCIL));
    }
}

fn result_detail(buf: &mut Buffer, state: &State, id: ReleaseId, c: &Columns, y: u16, height: u16) {
    let detail = c.at.detail;
    let room = c.right - detail;
    let files = state.files.get(&id);
    if state.focus == Focus::Files {
        match files {
            Some(Files::Ready(tree)) => file_tree(buf, state, tree, c, y, height),
            Some(Files::Loading { since, .. }) => {
                margin(buf, c.at, y, "files", fg(PENCIL));
                let secs = state.now.duration_since(*since).as_secs();
                let parts = [("fetching the file list", fg(TEXT)), (state.spinner(), fg(TEXT))];
                let used = spans(buf, detail, y, room, &parts);
                put(buf, detail + used, y, room.saturating_sub(used), &format!(" {secs}s"), fg(PENCIL));
                let note = "A magnet link has no file list until a peer sends one.";
                put(buf, detail, y + 1, room, note, fg(PENCIL));
            }
            Some(Files::Failed(error)) => {
                margin(buf, c.at, y, "files", fg(ERROR));
                put(buf, detail, y, room, &error.message, fg(ERROR));
                put(buf, detail, y + 1, room, error.hint.as_deref().unwrap_or("Press enter to try again."), fg(PENCIL));
            }
            None => {}
        }
        return;
    }
    if matches!(id, ReleaseId::Torrent(t) if state.job(t).is_some()) {
        put(buf, detail, y, room, "in downloads · press d to see it", fg(PENCIL));
        return;
    }
    let Some(release) = state.release(id) else { return };
    let mut who: Vec<String> = release.sources.clone();
    who.extend(release.flags.iter().map(|f| flag_words(*f).to_string()));
    let mut what = Vec::new();
    let quality = release.quality().to_string();
    if !quality.is_empty() {
        what.push(quality);
    }
    if c.at.narrow() {
        if let Some(n) = release.seeders {
            what.push(count(n as usize, "seed"));
        }
        if let Some(published) = release.published {
            what.push(match age(published, state.clock).as_str() {
                "now" => "just published".into(),
                ago => format!("{ago} ago"),
            });
        }
    }
    match files {
        Some(Files::Ready(tree)) => what.push(count(tree.files.len(), "file")),
        Some(Files::Loading { .. }) => what.push(format!("counting files{}", state.spinner())),
        Some(Files::Failed(_)) | None => {}
    }
    let (line, used) = if c.at.narrow() {
        put(buf, detail, y, room, &who.join(" · "), fg(PENCIL));
        (y + 1, put(buf, detail, y + 1, room, &what.join(" · "), fg(PENCIL)))
    } else {
        who.extend(what);
        (y, put(buf, detail, y, room, &who.join(" · "), fg(PENCIL)))
    };
    if let Some(Files::Failed(error)) = files {
        put(buf, detail + used + 3, line, room.saturating_sub(used + 3), &error.message, fg(ERROR));
    }
}

fn file_tree(buf: &mut Buffer, state: &State, tree: &Tree, c: &Columns, y: u16, height: u16) {
    let rows = usize::from(height - 2);
    for (k, &n) in tree.visible().iter().skip(tree.scroll).take(rows).enumerate() {
        let row = y + k as u16;
        if k == 0 {
            margin(buf, c.at, row, "files", fg(PENCIL));
        }
        let node = &tree.nodes[n];
        let here = n == tree.cursor;
        if here {
            mark(buf, c.at, row);
        }
        let at = c.at.detail + node.depth * 2;
        let check = tree.check(n);
        let (tick, tick_color) = match check {
            Check::All => ("✓", ACTION),
            Check::Some => ("–", ACTION),
            Check::None => ("·", PENCIL),
        };
        put(buf, at, row, 1, tick, fg(tick_color));
        let mut end = before_size(c);
        let name_x = match node.kind {
            NodeKind::Folder { .. } => {
                put(buf, at + 2, row, 1, if tree.folded.contains(&n) { "▸" } else { "▾" }, fg(PENCIL));
                at + 4
            }
            NodeKind::File(i) => {
                put_right(buf, c.size, row, &size(tree.files[i].size), fg(PENCIL));
                if tree.is_risky(i) {
                    end = put_right(buf, end, row, "executable", fg(ERROR)).saturating_sub(2);
                }
                at + 2
            }
        };
        let style = match (here, check) {
            (true, _) => bold(INK),
            (false, Check::None) => fg(PENCIL),
            (false, _) => fg(TEXT),
        };
        let room = end.saturating_sub(name_x);
        put(buf, name_x, row, room, &middle(&node.name, room as usize), style);
    }
    let ruled = y + rows as u16;
    let left_out = tree.risky_left_out();
    if left_out > 0 {
        let text = format!("{} left out", count(left_out, "executable"));
        put(buf, c.at.detail, ruled, before_size(c) - c.at.detail, &text, fg(ERROR));
    }
    put_right(buf, c.size, ruled, "────────", fg(PENCIL));
    let total = ruled + 1;
    let picked = tree.picked_count();
    let summary = if picked == 0 {
        "nothing picked".to_string()
    } else {
        format!("{picked} of {}", count(tree.files.len(), "file"))
    };
    let used = put(buf, c.at.detail, total, before_size(c) - c.at.detail, &summary, fg(TEXT));
    if let Some(free) = state.free_space {
        let picked = tree.picked_size();
        let (text, style) = match picked.checked_sub(free) {
            Some(short) if short > 0 => (format!(" · {} short of space", size(short)), fg(ERROR)),
            _ => (format!(" · {} free", size(free)), fg(PENCIL)),
        };
        put(buf, c.at.detail + used, total, before_size(c).saturating_sub(c.at.detail + used), &text, style);
    }
    put_right(buf, c.size, total, &size(tree.picked_size()), bold(INK));
}

fn empty(buf: &mut Buffer, state: &State, screen: &Screen, y: u16) {
    let at = screen.at;
    let room = screen.right - at.content;
    let mut lines = |word: &str, first: &str, alarm: bool, second: &str, second_color: Color| {
        margin(buf, at, y, word, fg(if alarm { ERROR } else { PENCIL }));
        put(buf, at.content, y, room, first, fg(if alarm { ERROR } else { TEXT }));
        put(buf, at.content, y + 1, room, second, fg(second_color));
    };
    if let (Engine::Unreachable(error), None) = (&state.engine, &state.search) {
        let why = error.hint.as_deref().unwrap_or("Downloads run in the engine, so this screen needs it.");
        lines("engine", "Cannot reach the engine.", true, why, PENCIL);
        return;
    }
    let Some(search) = &state.search else { return };
    if state.filtering() {
        let first = format!("Nothing loaded matches \"{}\".", state.query.text.trim());
        lines("found", &first, false, "Press enter to search for it.", PENCIL);
        return;
    }
    let failures: Vec<String> = search
        .sources
        .iter()
        .filter(|s| !s.outcome.is_ok())
        .map(|s| format!("{} {}", s.source, reason(&s.outcome)))
        .collect();
    let nothing = format!("Nothing found for \"{}\".", search.text);
    match &search.phase {
        SearchPhase::Running { .. } => {}
        SearchPhase::Done if failures.is_empty() => {
            lines("found", &nothing, false, "Try fewer or different words.", PENCIL)
        }
        SearchPhase::Done => lines("found", &nothing, false, &failures.join(" · "), ERROR),
        SearchPhase::Failed(error) => {
            let hint = error.hint.as_deref().unwrap_or("Check the connection, then press enter to search again.");
            lines("found", "No source answered.", true, hint, PENCIL);
        }
    }
}

const HELP: [(&str, &[(&str, &str)]); 6] = [
    ("move", &[("j k", "up, down"), ("g G", "first, last"), ("pgup pgdn", "a page"), ("d r", "downloads, results")]),
    ("typing", &[("/", "search"), ("enter", "search again"), ("ctrl+u", "clear"), ("tab", "into the list")]),
    ("results", &[("enter", "files"), ("D", "download"), ("s", "sort"), ("h l", "close, open")]),
    ("files", &[("space", "pick"), ("a", "all or none"), ("h l", "fold folders"), ("enter", "download")]),
    (
        "download",
        &[
            ("enter", "files"),
            ("space", "pause, resume"),
            ("v", "verify"),
            ("x", "remove"),
            ("o", "open"),
            ("y", "copy path"),
        ],
    ),
    ("always", &[("? f1", "keys"), ("esc", "back"), ("S", "ranking"), ("q", "quit"), ("ctrl+c", "quit")]),
];

const NOTES: [&str; 4] = [
    "Typing filters the results already loaded.",
    "Quitting leaves downloads running.",
    "Downloads stop at 100% and never seed.",
    "Removing a download keeps its files.",
];

fn help(buf: &mut Buffer, screen: &Screen) {
    let at = screen.at;
    let area = screen.list;
    rule(buf, at, area.y, area.bottom());
    let room = screen.right - at.content;
    let start = if at.narrow() { at.detail } else { at.content };
    let mut y = area.y;
    for (word, pairs) in HELP {
        if at.narrow() {
            if y >= area.bottom() {
                return;
            }
            put(buf, at.content, y, room, word, fg(PENCIL));
            y += 1;
        }
        if y >= area.bottom() {
            return;
        }
        margin(buf, at, y, word, fg(PENCIL));
        let mut x = start;
        for (key, verb) in pairs {
            let w = (width(key) + 1 + width(verb)) as u16;
            if x > start && x + w > at.content + room {
                y += 1;
                x = start;
            }
            x += put(buf, x, y, room, key, fg(INK));
            x += put(buf, x, y, room, &format!(" {verb}"), fg(PENCIL)) + 3;
        }
        y += 1;
    }
    for (k, note) in NOTES.iter().enumerate() {
        put(buf, at.content, y + 1 + k as u16, room, note, fg(PENCIL));
    }
}

fn footer(buf: &mut Buffer, state: &State, screen: &Screen) {
    let y = screen.footer;
    let pause = match state.cursor.and_then(|item| match item {
        Item::Download(id) => state.job(id),
        Item::Result(_) => None,
    }) {
        Some(job) if matches!(job.status, JobStatus::Paused | JobStatus::Interrupted | JobStatus::Error { .. }) => {
            "resume"
        }
        _ => "pause",
    };
    let downloaded = matches!(state.cursor, Some(Item::Result(ReleaseId::Torrent(t))) if state.job(t).is_some());
    let (sideways, down) = if screen.narrow() { ("h l", "tab") } else { ("← →", "↓") };
    let pairs: Vec<(&str, &str)> = match (state.focus, state.cursor) {
        _ if state.confirm.is_some() => Vec::new(),
        _ if state.ranking.is_some() => vec![("enter", "save"), ("esc", "close"), (sideways, "choose")],
        _ if state.help => vec![("esc", "close")],
        (Focus::Query, _) if state.items().is_empty() => vec![("enter", "search"), ("f1", "keys")],
        (Focus::Query, _) => vec![("enter", "search"), (down, "list"), ("f1", "keys")],
        (Focus::Files, Some(Item::Download(_))) => {
            vec![("space", pause), ("esc", "back"), ("o", "open"), ("?", "keys")]
        }
        (Focus::Files, _) => {
            vec![("space", "pick"), ("enter", "download"), ("a", "all"), ("esc", "back"), ("?", "keys")]
        }
        (Focus::List, Some(Item::Download(_))) => vec![("space", pause), ("x", "remove"), ("o", "open"), ("?", "keys")],
        (Focus::List, Some(Item::Result(_))) if downloaded => vec![("d", "downloads"), ("s", "sort"), ("?", "keys")],
        (Focus::List, Some(Item::Result(_))) => {
            vec![("enter", "files"), ("D", "download"), ("s", "sort"), ("?", "keys")]
        }
        (Focus::List, None) => vec![("/", "search"), ("?", "keys")],
    };
    let mut room = screen.right - screen.at.content;
    let essential = |(key, _): &(&str, &str)| state.toast.is_none() && matches!(*key, "esc" | "?" | "f1");
    let kept: Vec<(&str, &str)> =
        pairs.iter().enumerate().filter(|(i, pair)| *i == 0 || essential(pair)).map(|(_, pair)| *pair).collect();
    let reserve = |pairs: &[(&str, &str)]| if pairs.is_empty() { 0 } else { hints_width(pairs) + 4 };
    let first = reserve(&kept);
    let must = reserve(&pairs.iter().copied().filter(essential).collect::<Vec<_>>());
    let note = match (&state.toast, &state.upgrade) {
        (Some(toast), _) => {
            let color = match toast.tone {
                Tone::Info => TEXT,
                Tone::Success => ACTION,
                Tone::Warning | Tone::Error => ERROR,
            };
            Some((toast.text.clone(), color, first))
        }
        (None, Some(Upgrade::Available(latest))) => {
            let texts = match state.focus {
                Focus::Query => vec![format!("torrential {latest} is out"), format!("{latest} is out")],
                _ => vec![
                    format!("torrential {latest} is out · U update"),
                    format!("{latest} is out · U update"),
                    format!("U update to {latest}"),
                ],
            };
            let shortest = texts.last().filter(|text| width(text) as u16 + must <= room).cloned();
            first_fit(texts, room.saturating_sub(first).into()).or(shortest).map(|text| (text, PENCIL, must))
        }
        (None, Some(Upgrade::Installing(latest))) => {
            Some((format!("Updating to {latest}{}", state.spinner()), PENCIL, first))
        }
        (None, Some(Upgrade::Installed) | None) => None,
    };
    if let Some((text, color, reserved)) = note {
        let used = put(buf, screen.at.content, y, room.saturating_sub(reserved), &text, fg(color));
        room = room.saturating_sub(used + 4);
    }
    if !pairs.is_empty() {
        hints(buf, screen.right, y, room, &pairs, &kept);
    }
}
