//! Films and shows: the home screen with what to go on with and the saved titles, a search for
//! titles by name, and one title's screen with its seasons and episodes.

pub mod view;

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use torrential_core::protocol::{Home, TitlePage, TitlePlayed};
use torrential_core::text::sanitize;
use torrential_core::title::Episode;
use torrential_core::units::count;
use torrential_core::{AttemptEvent, Check, EpisodeInfo, Error, TitleCard, TitleId, VideoRef};

use crate::layout;
use crate::state::{Focus, Input, Page, State, Tone};
use crate::update::{Action, Effect, Move, edit};

/// Something asked of the engine: on its way, here, or failed.
#[derive(Clone, PartialEq, Debug)]
pub enum Load<T> {
    Loading,
    Ready(T),
    Failed(Error),
}

/// A search for films and shows by name.
#[derive(Clone, PartialEq, Debug)]
pub struct Found {
    pub id: u64,
    pub words: String,
    pub titles: Load<Vec<TitleCard>>,
}

/// A row the cursor rests on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Pick {
    /// A film or an episode to go on with. Enter plays it.
    Continue(VideoRef),
    /// A saved title. Enter opens it.
    Saved(TitleId),
    /// A title the search found. Enter opens it.
    Found(TitleId),
    /// The open title's Play row, which plays what comes next as `torrential play <title id>` does.
    Next,
    Episode(Episode),
}

/// The screen of one film or series.
#[derive(Clone, PartialEq, Debug)]
pub struct Opened {
    pub card: TitleCard,
    pub page: Load<TitlePage>,
    pub cursor: Pick,
    pub scroll: u16,
}

/// The last play from these screens, while it starts or after it failed. One that plays is gone,
/// and a toast says so.
#[derive(Clone, PartialEq, Debug)]
pub enum Play {
    /// What is starting, and the step it waits on.
    Starting { name: String, step: String },
    /// Why it did not play and what to do, in place of the list until Esc or a search. It scrolls
    /// on its own, since none of its rows takes the cursor.
    Failed { error: Error, scroll: u16 },
}

pub struct Titles {
    pub query: Input,
    /// Keys type into the search box.
    pub typing: bool,
    pub home: Load<Home>,
    /// The last search. While there is one, it shows in place of the home lists.
    pub found: Option<Found>,
    pub cursor: Option<Pick>,
    pub scroll: u16,
    /// The title screen, over the home lists or the search.
    pub open: Option<Opened>,
    pub play: Option<Play>,
    next_search: u64,
    /// The last request for the home lists, and for the open title's page. Only the answer to the
    /// last one is used: an earlier one may answer later.
    home_ask: u64,
    title_ask: u64,
}

impl Titles {
    pub fn new() -> Self {
        Self {
            query: Input::default(),
            typing: true,
            home: Load::Loading,
            found: None,
            cursor: None,
            scroll: 0,
            open: None,
            play: None,
            next_search: 1,
            home_ask: 0,
            title_ask: 0,
        }
    }

    pub(crate) fn ask_home(&mut self) -> Effect {
        self.home_ask += 1;
        Effect::Home { ask: self.home_ask }
    }

    /// The rows the cursor moves over, top to bottom.
    pub fn picks(&self) -> Vec<Pick> {
        if let Some(open) = &self.open {
            let episodes = match &open.page {
                Load::Ready(page) => episodes(page),
                _ => Vec::new(),
            };
            return std::iter::once(Pick::Next).chain(episodes.into_iter().map(|e| Pick::Episode(e.episode))).collect();
        }
        if let Some(found) = &self.found {
            return match &found.titles {
                Load::Ready(cards) => cards.iter().map(|card| Pick::Found(card.id.clone())).collect(),
                _ => Vec::new(),
            };
        }
        let Load::Ready(home) = &self.home else { return Vec::new() };
        let going_on = home.going_on.iter().map(|c| Pick::Continue(c.video.clone()));
        going_on.chain(home.saved.iter().map(|card| Pick::Saved(card.id.clone()))).collect()
    }

    /// The title a row belongs to.
    pub fn card(&self, pick: &Pick) -> Option<&TitleCard> {
        let home = match &self.home {
            Load::Ready(home) => Some(home),
            _ => None,
        };
        match pick {
            Pick::Continue(video) => home?.going_on.iter().find(|c| c.video == *video).map(|c| &c.title),
            Pick::Saved(id) => home?.saved.iter().find(|card| card.id == *id),
            Pick::Found(id) => match &self.found.as_ref()?.titles {
                Load::Ready(cards) => cards.iter().find(|card| card.id == *id),
                _ => None,
            },
            Pick::Next | Pick::Episode(_) => self.open.as_ref().map(|open| &open.card),
        }
    }

    /// How far down the rows that show are scrolled: a failed play's, the open title's, or the
    /// home lists' and the search's.
    pub fn scroll(&self) -> u16 {
        match (&self.play, &self.open) {
            (Some(Play::Failed { scroll, .. }), _) => *scroll,
            (_, Some(open)) => open.scroll,
            _ => self.scroll,
        }
    }

    fn scroll_mut(&mut self) -> &mut u16 {
        match (&mut self.play, &mut self.open) {
            (Some(Play::Failed { scroll, .. }), _) => scroll,
            (_, Some(open)) => &mut open.scroll,
            _ => &mut self.scroll,
        }
    }

    pub fn spinning(&self) -> bool {
        let searching = self.found.as_ref().is_some_and(|f| f.titles == Load::Loading);
        let opening = self.open.as_ref().is_some_and(|o| o.page == Load::Loading);
        searching || opening || matches!(self.play, Some(Play::Starting { .. }))
    }
}

/// A series' episodes in the order a person watches them: each season in turn, specials last.
/// A film has none.
pub fn episodes(page: &TitlePage) -> Vec<&EpisodeInfo> {
    let mut seasons: Vec<_> = page.details.seasons.iter().collect();
    seasons.sort_by_key(|season| (season.number == 0, season.number));
    seasons.into_iter().flat_map(|season| &season.episodes).collect()
}

/// An episode as people write it, such as S03E05.
pub fn label(episode: Episode) -> String {
    format!("S{:02}E{:02}", episode.season, episode.episode)
}

/// A film's name and year, or a series' name and the episode, as `Harbor Lights S01E02`.
pub fn title_name(card: &TitleCard, episode: Option<Episode>) -> String {
    match (episode, card.year) {
        (Some(episode), _) => format!("{} {}", card.name, label(episode)),
        (None, Some(year)) => format!("{} ({year})", card.name),
        (None, None) => card.name.clone(),
    }
}

/// Seconds into a video as a clock, as `34:10` or `1:02:05`.
pub fn clock(seconds: f64) -> String {
    let secs = Duration::try_from_secs_f64(seconds).map_or(0, |d| d.as_secs());
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// What a play's step says it waits on, or `None` for a step that changes nothing a person sees.
fn step_text(step: &AttemptEvent) -> Option<String> {
    Some(match step {
        AttemptEvent::Resolved { video, title, .. } => {
            format!("Finding sources of {}", title_name(title, video.episode))
        }
        AttemptEvent::Searched { found, .. } => format!("Found {}", count(*found as usize, "source")),
        AttemptEvent::Trying { rank, of, release } => format!("Trying {rank} of {of}: {}", sanitize(&release.name)),
        AttemptEvent::Check { check: Check::FileList } => "Waiting for its file list".into(),
        AttemptEvent::Check { check: Check::Start } => "Starting the download".into(),
        AttemptEvent::Check { check: Check::FirstBytes } => "Waiting for the first bytes of the video".into(),
        AttemptEvent::Playing { .. } => "Opening the player".into(),
        AttemptEvent::Skipped { .. } | AttemptEvent::Failed { .. } => return None,
    })
}

impl State {
    pub(crate) fn titles_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        use KeyCode::*;
        if self.titles.typing {
            return self.titles_query_key(key);
        }
        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return Vec::new();
        }
        let failed = matches!(self.titles.play, Some(Play::Failed { .. }));
        match key.code {
            Char('q') => self.quit = true,
            Char('?') => self.help = !self.help,
            Esc if self.help => self.help = false,
            Char('/') | Tab | BackTab => self.type_titles(),
            Char('d') => return self.show_downloads(),
            Char('U') => return self.act(Action::Upgrade),
            Down | Char('j') => self.move_pick(Move::Down),
            Up | Char('k') => self.move_pick(Move::Up),
            PageDown => self.move_pick(Move::PageDown),
            PageUp => self.move_pick(Move::PageUp),
            Home | Char('g') => self.move_pick(Move::First),
            End | Char('G') => self.move_pick(Move::Last),
            Esc => return self.back_titles(),
            _ if failed => {}
            Enter => return self.enter_pick(),
            Char('P') => return self.play_pick(),
            Right | Char('l') => return self.open_pick(),
            Left | Char('h') if self.titles.open.is_some() => return self.close_title(),
            _ => {}
        }
        Vec::new()
    }

    /// Esc clears the words in the search box, and in an empty box steps out of it: back to the
    /// title it was opened over, or from a search to the home lists.
    fn titles_query_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        use KeyCode::*;
        match key.code {
            Enter => return self.find_titles(),
            Esc if self.help => self.help = false,
            Esc if !self.titles.query.text.is_empty() => self.titles.query = Input::default(),
            Esc if self.titles.open.is_none() && self.titles.found.is_some() => {
                self.titles.typing = false;
                return self.back_titles();
            }
            Esc | Down | Tab | BackTab => self.titles.typing = false,
            _ => edit(&mut self.titles.query, key),
        }
        Vec::new()
    }

    fn type_titles(&mut self) {
        self.titles.typing = true;
        self.titles.query.caret = self.titles.query.text.chars().count();
    }

    /// Shows the films and shows, and asks the engine again for what to go on with.
    pub(crate) fn show_titles(&mut self) -> Vec<Effect> {
        self.page = Page::Titles;
        self.help = false;
        self.toast = None;
        vec![self.titles.ask_home()]
    }

    fn show_downloads(&mut self) -> Vec<Effect> {
        self.page = Page::Downloads;
        self.help = false;
        let effects = self.act(Action::Downloads);
        let back = if self.focus == Focus::Query { "esc" } else { "t" };
        self.say(Tone::Info, format!("Press {back} for films and shows."));
        effects
    }

    fn find_titles(&mut self) -> Vec<Effect> {
        let words = sanitize(self.titles.query.text.trim());
        let titles = &mut self.titles;
        if matches!(titles.play, Some(Play::Failed { .. })) {
            titles.play = None;
        }
        titles.typing = false;
        titles.open = None;
        titles.scroll = 0;
        titles.cursor = None;
        if words.is_empty() {
            titles.found = None;
            titles.query = Input::default();
            return Vec::new();
        }
        let id = titles.next_search;
        titles.next_search += 1;
        titles.query = Input { caret: words.chars().count(), text: words.clone() };
        titles.found = Some(Found { id, words: words.clone(), titles: Load::Loading });
        vec![Effect::FindTitles { search: id, words }]
    }

    /// Esc: away with a failed play's note, from a title to the list it opened from, from a search
    /// to the home lists, and from the home lists into the search box.
    fn back_titles(&mut self) -> Vec<Effect> {
        if matches!(self.titles.play, Some(Play::Failed { .. })) {
            self.titles.play = None;
            return Vec::new();
        }
        if self.titles.open.is_some() {
            return self.close_title();
        }
        let titles = &mut self.titles;
        if titles.found.take().is_some() {
            titles.query = Input::default();
            titles.cursor = None;
            titles.scroll = 0;
        } else {
            self.type_titles();
        }
        Vec::new()
    }

    fn close_title(&mut self) -> Vec<Effect> {
        self.titles.open = None;
        vec![self.titles.ask_home()]
    }

    /// Moves the cursor, or scrolls a failed play's note, which has no row for it to rest on.
    fn move_pick(&mut self, m: Move) {
        let page = layout::list_height(self).saturating_sub(2).max(1) as isize;
        let delta = match m {
            Move::Up => -1,
            Move::Down => 1,
            Move::PageUp => -page,
            Move::PageDown => page,
            Move::First => isize::MIN / 2,
            Move::Last => isize::MAX / 2,
        };
        if let Some(Play::Failed { scroll, .. }) = &mut self.titles.play {
            *scroll = (*scroll as isize + delta).clamp(0, u16::MAX as isize) as u16;
            return;
        }
        let picks = self.titles.picks();
        if picks.is_empty() {
            return;
        }
        let at = self.pick().and_then(|pick| picks.iter().position(|p| *p == pick));
        let to = at.map_or(0, |at| at.saturating_add_signed(delta).min(picks.len() - 1));
        self.set_pick(picks[to].clone());
    }

    /// Where the cursor is on the titles screen that shows.
    pub fn pick(&self) -> Option<Pick> {
        match &self.titles.open {
            Some(open) => Some(open.cursor.clone()),
            None => self.titles.cursor.clone(),
        }
    }

    fn set_pick(&mut self, pick: Pick) {
        match &mut self.titles.open {
            Some(open) => open.cursor = pick,
            None => self.titles.cursor = Some(pick),
        }
    }

    fn enter_pick(&mut self) -> Vec<Effect> {
        match self.pick() {
            Some(Pick::Saved(_) | Pick::Found(_)) => self.open_pick(),
            Some(Pick::Continue(_) | Pick::Next | Pick::Episode(_)) => self.play_pick(),
            None => Vec::new(),
        }
    }

    fn open_pick(&mut self) -> Vec<Effect> {
        let Some(pick) = self.pick().filter(|p| !matches!(p, Pick::Next | Pick::Episode(_))) else { return Vec::new() };
        let Some(card) = self.titles.card(&pick).cloned() else { return Vec::new() };
        let id = card.id.clone();
        self.titles.open = Some(Opened { card, page: Load::Loading, cursor: Pick::Next, scroll: 0 });
        self.titles.title_ask += 1;
        vec![Effect::Title { ask: self.titles.title_ask, id }]
    }

    fn play_pick(&mut self) -> Vec<Effect> {
        let Some(pick) = self.pick() else { return Vec::new() };
        let Some(card) = self.titles.card(&pick).cloned() else { return Vec::new() };
        let page = match &self.titles.open {
            Some(Opened { page: Load::Ready(page), .. }) => Some(page),
            _ => None,
        };
        // A title alone plays what comes next, as `torrential play <title id>` does.
        let episode = match &pick {
            Pick::Continue(video) => video.episode,
            Pick::Saved(_) | Pick::Found(_) | Pick::Next => None,
            Pick::Episode(episode) => {
                let info = page.and_then(|page| episodes(page).into_iter().find(|info| info.episode == *episode));
                if info.is_some_and(|info| info.aired.is_none_or(|aired| aired > self.clock)) {
                    self.say(Tone::Info, format!("{} has not aired yet.", label(*episode)));
                    return Vec::new();
                }
                Some(*episode)
            }
        };
        if let Some(Play::Starting { name, .. }) = &self.titles.play {
            self.say(Tone::Info, format!("Still starting {name}."));
            return Vec::new();
        }
        let shown = match &pick {
            Pick::Next => page.and_then(|page| page.next.as_ref()?.video.episode),
            _ => episode,
        };
        let name = title_name(&card, shown);
        self.titles.play = Some(Play::Starting { name, step: "Looking up the title".into() });
        vec![Effect::PlayTitle(VideoRef { title: card.id, episode })]
    }

    pub(crate) fn home_arrived(&mut self, ask: u64, result: Result<Home, Error>) {
        if ask != self.titles.home_ask {
            return;
        }
        self.titles.home = match result {
            Ok(home) => Load::Ready(home),
            Err(error) => Load::Failed(error),
        };
        let titles = &mut self.titles;
        let untouched = !self.touched && titles.typing && titles.query.text.is_empty() && titles.found.is_none();
        if untouched && !titles.picks().is_empty() {
            titles.typing = false;
        }
    }

    pub(crate) fn titles_found(&mut self, search: u64, result: Result<Vec<TitleCard>, Error>) {
        let Some(found) = self.titles.found.as_mut().filter(|f| f.id == search) else { return };
        found.titles = match result {
            Ok(cards) => Load::Ready(cards),
            Err(error) => Load::Failed(error),
        };
    }

    pub(crate) fn title_arrived(&mut self, ask: u64, result: Result<TitlePage, Error>) {
        if ask != self.titles.title_ask {
            return;
        }
        let Some(open) = self.titles.open.as_mut() else { return };
        open.page = match result {
            Ok(page) => {
                open.card = page.details.card.clone();
                Load::Ready(page)
            }
            Err(error) => Load::Failed(error),
        };
    }

    pub(crate) fn play_step(&mut self, step: AttemptEvent) {
        if let (Some(Play::Starting { step: shown, .. }), Some(text)) = (self.titles.play.as_mut(), step_text(&step)) {
            *shown = text;
        }
    }

    pub(crate) fn title_played(&mut self, result: Result<(String, TitlePlayed), Error>) {
        self.titles.play = None;
        match result {
            Ok((player, played)) => match played.resume_at {
                Some(at) => self.say(Tone::Success, format!("Resuming {} at {} in {player}", played.name, clock(at))),
                None => self.say(Tone::Success, format!("Playing {} in {player}", played.name)),
            },
            Err(error) => {
                self.say(Tone::Error, format!("Playback failed: {}", error.message));
                self.titles.play = Some(Play::Failed { error, scroll: 0 });
            }
        }
    }

    /// Keeps the cursor on a row that shows, and that row inside the list.
    pub(crate) fn settle_titles(&mut self) {
        let picks = self.titles.picks();
        let titles = &mut self.titles;
        match &mut titles.open {
            Some(open) if !picks.contains(&open.cursor) => open.cursor = Pick::Next,
            Some(_) => {}
            None if titles.cursor.as_ref().is_none_or(|c| !picks.contains(c)) => titles.cursor = picks.first().cloned(),
            None => {}
        }
        let Some(screen) = layout::screen(self.size) else { return };
        let lines = view::lines(self, &screen);
        let pick = self.pick();
        let at = pick.and_then(|pick| lines.iter().position(|line| line.pick.as_ref() == Some(&pick)));
        let total = lines.len() as u16;
        let scroll = self.titles.scroll_mut();
        *scroll = layout::settle(total, at.map(|at| (at as u16, 1)), i32::from(*scroll), screen.list.height);
    }
}
