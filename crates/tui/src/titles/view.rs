//! How the films and shows draw: one list of rows under the search line, scrolled to keep the
//! cursor in view.

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::style::{Color, Style};
use torrential_core::protocol::TitlePage;
use torrential_core::units::{count, format_duration};
use torrential_core::{Kind, Next, TitleCard, VideoRef};

use super::{Found, Load, Opened, Pick, episodes, label, title_name};
use crate::format::{age, width};
use crate::layout::Screen;
use crate::state::{Engine, State};
use crate::theme::*;
use crate::view::{arrow, bold, fg, margin, mark, rule, search_box, spans, spans_right};

/// The most rows a synopsis takes. The last one ends in an ellipsis when there is more.
const SYNOPSIS_ROWS: usize = 3;

/// One row of the list, ready to draw.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Line {
    /// The word in the margin, on the first row of a group.
    pub word: Option<(String, Color)>,
    /// What the row plays or opens, when the cursor can rest on it.
    pub pick: Option<Pick>,
    pub left: Vec<(String, Style)>,
    pub right: Vec<(String, Style)>,
}

/// Rows as they are added. A group's word goes in the margin of its first row, or on a row of
/// its own on a screen too narrow for a margin.
struct Sheet {
    narrow: bool,
    word: Option<(String, Color)>,
    lines: Vec<Line>,
}

impl Sheet {
    fn word(&mut self, word: impl Into<String>, color: Color) {
        let word = word.into();
        if self.narrow {
            self.lines.push(Line { left: vec![(word, fg(color))], ..Line::default() });
        } else {
            self.word = Some((word, color));
        }
    }

    fn row(&mut self, pick: Option<Pick>, left: Vec<(String, Style)>, right: Vec<(String, Style)>) {
        self.lines.push(Line { word: self.word.take(), pick, left, right });
    }

    fn text(&mut self, text: impl Into<String>, color: Color) {
        self.row(None, vec![(text.into(), fg(color))], Vec::new());
    }

    fn gap(&mut self) {
        if self.lines.last().is_some_and(|line| *line != Line::default()) {
            self.lines.push(Line::default());
        }
    }

    /// A sentence that stands in for a list: what happened, and what to do about it.
    fn note(&mut self, word: &str, alarm: bool, first: &str, second: &str) {
        self.word(word, if alarm { ERROR } else { PENCIL });
        self.text(first, if alarm { ERROR } else { TEXT });
        self.text(second, PENCIL);
    }
}

/// Every row of the titles screen that shows, top to bottom.
pub fn lines(state: &State, screen: &Screen) -> Vec<Line> {
    let mut sheet = Sheet { narrow: screen.narrow(), word: None, lines: Vec::new() };
    let titles = &state.titles;
    match (&titles.open, &titles.found) {
        (Some(open), _) => title_lines(&mut sheet, state, open, screen.right - screen.at.content),
        (None, Some(found)) => found_lines(&mut sheet, found),
        (None, None) => home_lines(&mut sheet, state),
    }
    sheet.lines
}

fn home_lines(sheet: &mut Sheet, state: &State) {
    let home = match (&state.titles.home, &state.engine) {
        (Load::Ready(home), _) => home,
        (_, Engine::Unreachable(error)) => {
            let why = error.hint.as_deref().unwrap_or("Films and shows are unavailable until the engine is running.");
            return sheet.note("engine", true, "Cannot reach the engine.", why);
        }
        (Load::Failed(error), _) => {
            return sheet.note("titles", true, &error.message, error.hint.as_deref().unwrap_or_default());
        }
        (Load::Loading, _) => return,
    };
    if home.going_on.is_empty() && home.saved.is_empty() {
        sheet.text("Search for a film or a show to watch.", TEXT);
        sheet.text("It plays in your player while it downloads.", PENCIL);
        return;
    }
    if !home.going_on.is_empty() {
        sheet.word("continue", PENCIL);
    }
    for item in &home.going_on {
        let left = match &item.episode {
            Some(episode) => {
                let name = episode.name.as_deref().map(|name| format!(" · {name}")).unwrap_or_default();
                vec![(item.title.name.clone(), fg(TEXT)), (format!("  {}{name}", label(episode.episode)), fg(PENCIL))]
            }
            None => vec![(title_name(&item.title, None), fg(TEXT))],
        };
        let started = item.resume_at.zip(item.duration).filter(|(_, duration)| *duration > 0.0);
        let right = match (started, &item.episode) {
            (Some((at, duration)), _) => time_left(at, duration),
            (None, Some(_)) => "next episode".into(),
            (None, None) => "not started".into(),
        };
        sheet.row(Some(Pick::Continue(item.video.clone())), left, vec![(right, fg(PENCIL))]);
    }
    sheet.gap();
    if !home.saved.is_empty() {
        sheet.word("saved", PENCIL);
    }
    for card in &home.saved {
        sheet.row(Some(Pick::Saved(card.id.clone())), card_name(card), card_facts(card));
    }
}

fn found_lines(sheet: &mut Sheet, found: &Found) {
    match &found.titles {
        Load::Loading => {}
        Load::Failed(error) => {
            let hint = error.hint.as_deref().unwrap_or("Press enter to search again.");
            sheet.note("titles", true, &error.message, hint);
        }
        Load::Ready(cards) if cards.is_empty() => {
            let first = format!("No films or shows named \"{}\".", found.words);
            sheet.note("titles", false, &first, "Try fewer or different words.");
        }
        Load::Ready(cards) => {
            sheet.word("titles", PENCIL);
            for card in cards {
                sheet.row(Some(Pick::Found(card.id.clone())), card_name(card), card_facts(card));
            }
        }
    }
}

fn card_name(card: &TitleCard) -> Vec<(String, Style)> {
    vec![(card.name.clone(), fg(TEXT))]
}

/// A film's year, or a series' first year and that it is one.
fn card_facts(card: &TitleCard) -> Vec<(String, Style)> {
    let year = card.year.map(|year| year.to_string());
    let facts: Vec<String> = match card.kind {
        Kind::Film => year.into_iter().collect(),
        Kind::Series => year.into_iter().chain(["series".to_string()]).collect(),
    };
    vec![(facts.join(" · "), fg(PENCIL))]
}

fn title_lines(sheet: &mut Sheet, state: &State, open: &Opened, room: u16) {
    let kind = match open.card.kind {
        Kind::Film => "film",
        Kind::Series => "series",
    };
    sheet.word(kind, PENCIL);
    sheet.row(None, vec![(open.card.name.clone(), bold(INK))], Vec::new());
    let page = match &open.page {
        Load::Ready(page) => page,
        Load::Loading => return sheet.text(format!("looking up the title{}", state.spinner()), PENCIL),
        Load::Failed(error) => {
            sheet.text(error.message.clone(), ERROR);
            return sheet.text(error.hint.clone().unwrap_or_default(), PENCIL);
        }
    };
    let mut facts = about(page);
    while facts.len() > 1 && width(&facts.join(" · ")) > usize::from(room) {
        facts.pop();
    }
    sheet.text(facts.join(" · "), PENCIL);
    if let Some(synopsis) = &page.details.synopsis {
        for row in wrap(synopsis, room.into(), SYNOPSIS_ROWS) {
            sheet.text(row, TEXT);
        }
    }
    sheet.gap();
    play_row(sheet, page, state.clock);
    let mut season = None;
    for info in episodes(page) {
        if season != Some(info.episode.season) {
            season = Some(info.episode.season);
            sheet.gap();
            let word = match info.episode.season {
                0 => "specials".to_string(),
                n => format!("season {n}"),
            };
            sheet.word(word, PENCIL);
        }
        let video = VideoRef { title: page.details.card.id.clone(), episode: Some(info.episode) };
        let aired = info.aired.is_some_and(|aired| aired <= state.clock);
        let watched = page.watched.iter().find(|w| w.video == video);
        let mut facts = Vec::new();
        match (info.aired, watched) {
            (None, _) => facts.push("not announced".to_string()),
            (Some(aired), _) if aired > state.clock => facts.push(format!("airs in {}", age(state.clock, aired))),
            (_, Some(w)) if w.finished() => facts.push("watched".into()),
            (_, Some(w)) if w.position > 0.0 => {
                facts.extend(w.duration.filter(|d| *d > 0.0).map(|duration| time_left(w.position, duration)));
            }
            _ => {}
        }
        if page.next.as_ref().is_some_and(|next| next.video == video) {
            facts.push("up next".into());
        }
        let number = format!("{:>2}  ", info.episode.episode);
        let name = info.name.clone().unwrap_or_else(|| format!("Episode {}", info.episode.episode));
        let left = vec![(number, fg(PENCIL)), (name, fg(if aired { TEXT } else { PENCIL }))];
        sheet.row(Some(Pick::Episode(info.episode)), left, vec![(facts.join(" · "), fg(PENCIL))]);
    }
}

/// What a title is, in a few words: its year, how many seasons, how long, its genres, its rating.
fn about(page: &TitlePage) -> Vec<String> {
    let details = &page.details;
    let mut about: Vec<String> = details.card.year.map(|year| year.to_string()).into_iter().collect();
    let seasons = details.seasons.iter().filter(|season| season.number > 0).count();
    if details.card.kind == Kind::Series && seasons > 0 {
        about.push(count(seasons, "season"));
    }
    about.extend(details.runtime_min.map(|minutes| format!("{minutes} min")));
    if !details.genres.is_empty() {
        about.push(details.genres.join(", "));
    }
    about.extend(details.card.rating.map(|rating| format!("rated {rating:.1}")));
    about
}

/// The Play row: what plays, and how much of it is left when it resumes.
fn play_row(sheet: &mut Sheet, page: &TitlePage, now: i64) {
    let Some(Next { video, resume_at }) = page.next.clone().or_else(|| restart(page, now)) else {
        return sheet.row(Some(Pick::Next), vec![("Play".into(), fg(TEXT))], Vec::new());
    };
    let resuming = resume_at.filter(|at| *at > 0.0);
    let verb = if resuming.is_some() { "Resume" } else { "Play" };
    let left = match video.episode {
        None => vec![(verb.to_string(), fg(TEXT))],
        Some(episode) => {
            let info = episodes(page).into_iter().find(|info| info.episode == episode);
            let name = info.and_then(|info| info.name.clone()).map(|name| (format!("  {name}"), fg(PENCIL)));
            std::iter::once((format!("{verb} {}", label(episode)), fg(TEXT))).chain(name).collect()
        }
    };
    let duration = page.watched.iter().find(|w| w.video == video).and_then(|w| w.duration);
    let duration = duration.or(page.details.runtime_min.map(|minutes| f64::from(minutes) * 60.0));
    let right = resuming.zip(duration).map(|(at, duration)| (time_left(at, duration), fg(PENCIL)));
    sheet.row(Some(Pick::Next), left, right.into_iter().collect());
}

/// What Play starts once a film is finished or a series is caught up: the film again, or the first
/// aired episode.
fn restart(page: &TitlePage, now: i64) -> Option<Next> {
    let id = page.details.card.id.clone();
    let episode = match page.details.card.kind {
        Kind::Film => None,
        Kind::Series => Some(
            episodes(page)
                .into_iter()
                .find(|info| info.episode.season > 0 && info.aired.is_some_and(|a| a <= now))?
                .episode,
        ),
    };
    Some(Next { video: VideoRef { title: id, episode }, resume_at: None })
}

fn time_left(position: f64, duration: f64) -> String {
    let left = Duration::try_from_secs_f64(duration - position).unwrap_or_default();
    format!("{} left", format_duration(left))
}

/// `text` in rows at most `room` cells wide, broken between words. Past `most` rows, the last row
/// holds the rest, for `put` to end in an ellipsis.
fn wrap(text: &str, room: usize, most: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        let full = rows.len() == most;
        match rows.last_mut() {
            Some(row) if full || width(row) + 1 + width(word) <= room => {
                row.push(' ');
                row.push_str(word);
            }
            _ => rows.push(word.to_string()),
        }
    }
    rows
}

pub fn search(buf: &mut Buffer, state: &State, screen: &Screen) -> Option<Position> {
    let titles = &state.titles;
    let typing = titles.typing && !state.help;
    let hint = if typing { "type a film or a show, then press enter" } else { "press / to search" };
    let status = |short: bool| -> Vec<(String, Style)> {
        match titles.found.as_ref().map(|found| &found.titles) {
            None => Vec::new(),
            Some(Load::Loading) => vec![("searching".into(), fg(PENCIL)), (state.spinner().into(), fg(PENCIL))],
            Some(Load::Ready(cards)) => vec![(count(cards.len(), "title"), fg(PENCIL))],
            Some(Load::Failed(_)) => vec![((if short { "failed" } else { "search failed" }).into(), fg(ERROR))],
        }
    };
    search_box(buf, screen, &titles.query, typing, hint, [status(false), status(true)])
}

pub fn list(buf: &mut Buffer, state: &State, screen: &Screen) {
    let at = screen.at;
    let area = screen.list;
    rule(buf, at, area.y, area.bottom());
    let lines = lines(state, screen);
    let scroll = match &state.titles.open {
        Some(open) => open.scroll,
        None => state.titles.scroll,
    };
    let scroll = usize::from(scroll).min(lines.len().saturating_sub(area.height.into()));
    let cursor = state.pick().filter(|_| !state.titles.typing);
    for (line, y) in lines.iter().skip(scroll).zip(area.y..area.bottom()) {
        if let Some((word, color)) = &line.word {
            margin(buf, at, y, word, fg(*color));
        }
        let here = cursor.is_some() && line.pick == cursor;
        if here {
            mark(buf, at, y);
        }
        let end = if line.right.is_empty() { screen.right } else { spans_right(buf, screen.right, y, &line.right) };
        let room = end.saturating_sub(at.content + u16::from(!line.right.is_empty()) * 2);
        let left: Vec<(&str, Style)> = line
            .left
            .iter()
            .map(|(text, style)| (text.as_str(), if here && style.fg == Some(TEXT) { bold(INK) } else { *style }))
            .collect();
        spans(buf, at.content, y, room, &left);
    }
    if let Some((word, color)) = carried(&lines, scroll) {
        margin(buf, at, area.y, word, fg(*color));
    }
    if scroll > 0 {
        arrow(buf, at, area.y, "↑");
    }
    if scroll + usize::from(area.height) < lines.len() {
        arrow(buf, at, area.bottom() - 1, "↓");
    }
}

/// The word of the group the top row belongs to, when the list is scrolled past the row that
/// carries it. A gap ends a group.
fn carried(lines: &[Line], scroll: usize) -> Option<&(String, Color)> {
    let top = lines.get(scroll).filter(|line| line.word.is_none() && **line != Line::default())?;
    let before = lines[..scroll].iter().rev().take_while(|line| **line != Line::default());
    before.chain([top]).find_map(|line| line.word.as_ref())
}

/// The keys the footer offers on the titles screen, most useful first.
pub fn hints(state: &State, screen: &Screen) -> Vec<(&'static str, &'static str)> {
    let titles = &state.titles;
    let down = if screen.narrow() { "tab" } else { "↓" };
    if state.help {
        return vec![("esc", "close")];
    }
    if titles.typing {
        return match titles.picks().is_empty() {
            true => vec![("enter", "search"), ("f1", "keys")],
            false => vec![("enter", "search"), (down, "list"), ("f1", "keys")],
        };
    }
    match state.pick() {
        Some(Pick::Next | Pick::Episode(_)) => {
            vec![("enter", "play"), ("esc", "back"), ("d", "downloads"), ("?", "keys")]
        }
        Some(Pick::Continue(_)) => {
            vec![("enter", "play"), ("l", "open"), ("/", "search"), ("d", "downloads"), ("?", "keys")]
        }
        Some(Pick::Saved(_) | Pick::Found(_)) => {
            vec![("enter", "open"), ("P", "play"), ("/", "search"), ("d", "downloads"), ("?", "keys")]
        }
        None if titles.open.is_some() => vec![("esc", "back"), ("d", "downloads"), ("?", "keys")],
        None => vec![("/", "search"), ("d", "downloads"), ("?", "keys")],
    }
}

pub const HELP: [(&str, &[(&str, &str)]); 4] = [
    ("move", &[("j k", "up, down"), ("g G", "first, last"), ("pgup pgdn", "page")]),
    ("typing", &[("/", "search"), ("enter", "search"), ("ctrl+u", "clear"), ("tab", "into the list")]),
    ("titles", &[("enter", "play or open"), ("P", "play"), ("l", "open"), ("h", "close")]),
    ("always", &[("? f1", "keys"), ("esc", "back"), ("d", "downloads"), ("q", "quit"), ("ctrl+c", "quit")]),
];

pub const NOTES: [&str; 3] = [
    "A film or an episode plays in your player while it downloads.",
    "Play resumes where you stopped, or starts the next episode.",
    "Downloads keep running after you quit.",
];
