use crossterm::event::KeyCode;
use ratatui::style::Modifier;
use torrential_core::error::Details;
use torrential_core::protocol::{Continue, Home, TitlePage, TitlePlayed};
use torrential_core::title::Episode;
use torrential_core::{
    Art, AttemptEvent, Check, EpisodeInfo, Kind, Next, Season, Skip, TitleCard, TitleDetails, TitleId, Tried, VideoRef,
    Watched,
};

use super::render::{MARK, find, line, render, snapshots};
use super::*;
use crate::state::Page;
use crate::theme;
use crate::titles::{Load, Pick};

const DAY: i64 = 86_400;

fn video(text: &str) -> VideoRef {
    text.parse().unwrap()
}

fn title_id(text: &str) -> TitleId {
    text.parse().unwrap()
}

fn card(id: &str, kind: Kind, name: &str, year: u16, rating: f32) -> TitleCard {
    TitleCard { id: title_id(id), kind, name: name.into(), year: Some(year), rating: Some(rating) }
}

fn night() -> TitleCard {
    card("tt0063350", Kind::Film, "Night of the Living Dead", 1968, 7.8)
}

fn harbor() -> TitleCard {
    card("tt9999901", Kind::Series, "Harbor Lights", 2019, 8.1)
}

fn sintel() -> TitleCard {
    card("tt1727587", Kind::Film, "Sintel", 2010, 7.4)
}

fn episode(season: u32, number: u32, name: &str, aired: i64) -> EpisodeInfo {
    EpisodeInfo {
        episode: Episode { season, episode: number },
        name: Some(name.into()),
        aired: Some(aired),
        overview: None,
        thumbnail: false,
    }
}

fn details(card: TitleCard, synopsis: &str, runtime_min: u32, seasons: Vec<Season>) -> TitleDetails {
    TitleDetails {
        card,
        synopsis: Some(synopsis.into()),
        genres: vec!["Horror".into(), "Thriller".into()],
        runtime_min: Some(runtime_min),
        cast: Vec::new(),
        countries: Default::default(),
        art: Art::default(),
        seasons,
    }
}

fn watched(video_text: &str, position: f64, duration: f64, days_ago: i64) -> Watched {
    Watched { video: video(video_text), position, duration: Some(duration), release: None, at: CLOCK - days_ago * DAY }
}

fn harbor_page() -> TitlePage {
    let seasons = vec![
        Season { number: 0, episodes: vec![episode(0, 1, "Lighthouse Diaries", CLOCK - 300 * DAY)] },
        Season {
            number: 1,
            episodes: vec![
                episode(1, 1, "First Light", CLOCK - 400 * DAY),
                episode(1, 2, "Low Tide", CLOCK - 393 * DAY),
                episode(1, 3, "Third Light", CLOCK - 386 * DAY),
                episode(1, 4, "Fog Bank", CLOCK - 379 * DAY),
            ],
        },
        Season {
            number: 2,
            episodes: vec![episode(2, 1, "Return", CLOCK - 2 * DAY), episode(2, 2, "Undertow", CLOCK + 5 * DAY)],
        },
    ];
    TitlePage {
        details: details(
            harbor(),
            "A keeper and her daughter tend the last manned lighthouse on the coast.",
            44,
            seasons,
        ),
        saved: false,
        watched: vec![
            watched("tt9999901:1:1", 2630.0, 2640.0, 9),
            watched("tt9999901:1:2", 2640.0, 2640.0, 8),
            watched("tt9999901:1:3", 1320.0, 2640.0, 1),
        ],
        next: Some(Next { video: video("tt9999901:1:3"), resume_at: Some(1320.0) }),
        downloads: Vec::new(),
    }
}

fn night_page(watched_at: Option<f64>) -> TitlePage {
    let synopsis = "A ragtag group of Pennsylvanians barricade themselves in an old farmhouse to remain safe from a \
                    horde of flesh-eating ghouls that are ravaging the Northeast of the United States.";
    let watched: Vec<Watched> = watched_at.map(|at| watched("tt0063350", at, 5760.0, 3)).into_iter().collect();
    let next = match watched_at {
        Some(at) if at >= 5700.0 => None,
        at => Some(Next { video: video("tt0063350"), resume_at: at }),
    };
    TitlePage { details: details(night(), synopsis, 96, Vec::new()), saved: true, watched, next, downloads: Vec::new() }
}

fn home() -> Home {
    let third = episode(1, 3, "Third Light", CLOCK - 386 * DAY);
    Home {
        going_on: vec![
            Continue {
                title: harbor(),
                video: video("tt9999901:1:3"),
                episode: Some(third),
                resume_at: Some(1320.0),
                duration: Some(2640.0),
            },
            Continue {
                title: night(),
                video: video("tt0063350"),
                episode: None,
                resume_at: Some(2050.0),
                duration: Some(5760.0),
            },
        ],
        saved: vec![night(), sintel()],
    }
}

fn found() -> Vec<TitleCard> {
    vec![
        night(),
        card("tt0100258", Kind::Film, "Night of the Living Dead", 1990, 6.9),
        card("tt9999902", Kind::Series, "Night of the Living Dead: The Series", 2024, 5.2),
    ]
}

fn played(name: &str, video_text: &str, resume_at: Option<f64>) -> TitlePlayed {
    let release = super::night().remove(0);
    TitlePlayed { video: video(video_text), name: name.into(), release, tried: Vec::new(), resume_at }
}

/// Bare `torrential` once the engine has answered with no downloads and this home.
fn opened(size: (u16, u16)) -> State {
    let mut state = titles_app(size);
    state.update(Msg::Watch(Event::Snapshot { jobs: jobs() }));
    state.update(Msg::Home { ask: 1, result: Ok(home()) });
    state
}

/// Bare `torrential` once the engine has answered with no downloads and an empty home.
fn empty(size: (u16, u16)) -> State {
    let mut state = titles_app(size);
    state.update(Msg::Watch(Event::Snapshot { jobs: Vec::new() }));
    state.update(Msg::Home { ask: 1, result: Ok(Home { going_on: Vec::new(), saved: Vec::new() }) });
    state
}

fn searching(size: (u16, u16)) -> State {
    let mut state = opened(size);
    press(&mut state, "/night of the living dead");
    state.update(key(KeyCode::Enter));
    state.update(Msg::FoundTitles { search: 1, result: Ok(found()) });
    state
}

fn film(size: (u16, u16)) -> State {
    let mut state = searching(size);
    state.update(key(KeyCode::Enter));
    state.update(Msg::Title { ask: 1, result: Ok(Box::new(night_page(Some(2050.0)))) });
    state
}

fn series(size: (u16, u16)) -> State {
    let mut state = opened(size);
    press(&mut state, "l");
    state.update(Msg::Title { ask: 1, result: Ok(Box::new(harbor_page())) });
    state
}

fn list_lines(state: &State) -> Vec<String> {
    let buf = render(state);
    (4..state.size.1 - 2).map(|y| line(&buf, y)).collect()
}

fn footer(state: &State) -> String {
    line(&render(state), state.size.1 - 1)
}

#[test]
fn bare_torrential_opens_on_what_to_go_on_with_and_the_saved_titles() {
    let mut state = titles_app((100, 30));
    assert_eq!(state.page, Page::Titles);
    let effects = state.update(Msg::Watch(Event::Snapshot { jobs: jobs() }));
    assert!(effects.contains(&Effect::Home { ask: 1 }), "the engine is asked what to go on with: {effects:?}");
    state.update(Msg::Home { ask: 1, result: Ok(home()) });
    assert_eq!(
        list_lines(&state)[..6],
        [
            "   continue ● Harbor Lights  S01E03 · Third Light                                         22m left",
            "            │ Night of the Living Dead (1968)                                           1h01m left",
            "            │",
            "      saved │ Night of the Living Dead                                                        1968",
            "            │ Sintel                                                                          2010",
            "            │",
        ]
    );
    assert_eq!(line(&render(&state), 2), "     search │ press / to search");
    assert_eq!(footer(&state).trim(), "enter play   l open   / search   d downloads   ? keys");
    let buf = render(&state);
    let (x, y) = find(&buf, "Harbor Lights");
    assert_eq!((buf[(x, y)].fg, buf[(x, y)].modifier.contains(Modifier::BOLD)), (theme::INK, true));
    assert_eq!(buf[(MARK, y)].symbol(), "●");
}

#[test]
fn enter_on_what_you_were_watching_plays_it_from_where_you_stopped() {
    let mut state = opened((100, 30));
    let effects = state.update(key(KeyCode::Enter));
    assert_eq!(effects, [Effect::PlayTitle(video("tt9999901:1:3"))]);
    assert!(footer(&state).starts_with("              Looking up the title"), "{}", footer(&state));
    let trying = AttemptEvent::Trying { rank: 1, of: 3, release: super::night().remove(0) };
    state.update(Msg::PlayStep(Box::new(trying)));
    assert!(footer(&state).starts_with("              Trying 1 of 3: Night of the Living Dead (1968)"));
    state.update(Msg::PlayStep(Box::new(AttemptEvent::Check { check: Check::FirstBytes })));
    assert!(footer(&state).starts_with("              Waiting for the first bytes of the video"));
    assert_eq!(state.update(key(KeyCode::Enter)), [], "one play at a time");
    let resumed = played("Harbor Lights S01E03", "tt9999901:1:3", Some(1320.0));
    state.update(Msg::TitlePlayed(Ok(("player-stub".into(), Box::new(resumed)))));
    assert!(
        footer(&state).starts_with("              Resuming Harbor Lights S01E03 at 22:00 in player-stub"),
        "{}",
        footer(&state)
    );
    let failed = Error::new(ErrorCode::NoSource, "no source of Sintel (2010) started");
    press(&mut state, "jjj");
    assert_eq!(state.update(key(KeyCode::Char('P'))), [Effect::PlayTitle(video("tt1727587"))]);
    state.update(Msg::TitlePlayed(Err(failed)));
    assert!(footer(&state).starts_with("              Playback failed: no source of Sintel (2010) started"));
}

#[test]
fn a_search_finds_films_and_shows_by_name_and_enter_opens_one() {
    let mut state = opened((100, 30));
    press(&mut state, "/night of the living dead");
    assert_eq!(
        state.update(key(KeyCode::Enter)),
        [Effect::FindTitles { search: 1, words: "night of the living dead".into() }]
    );
    assert!(line(&render(&state), 2).ends_with("searching"), "{}", line(&render(&state), 2));
    state.update(Msg::FoundTitles { search: 1, result: Ok(found()) });
    assert_eq!(
        line(&render(&state), 2),
        "     search │ night of the living dead                                                    3 titles"
    );
    assert_eq!(
        list_lines(&state)[..3],
        [
            "     titles ● Night of the Living Dead                                                        1968",
            "            │ Night of the Living Dead                                                        1990",
            "            │ Night of the Living Dead: The Series                                   2024 · series",
        ]
    );
    assert_eq!(footer(&state).trim(), "enter open   P play   / search   d downloads   ? keys");
    assert_eq!(state.update(key(KeyCode::Enter)), [Effect::Title { ask: 1, id: title_id("tt0063350") }]);
    state.update(Msg::Title { ask: 1, result: Ok(Box::new(night_page(Some(2050.0)))) });
    assert_eq!(
        list_lines(&state)[..7],
        [
            "       film │ Night of the Living Dead",
            "            │ 1968 · 96 min · Horror, Thriller · rated 7.8",
            "            │ A ragtag group of Pennsylvanians barricade themselves in an old farmhouse to remain",
            "            │ safe from a horde of flesh-eating ghouls that are ravaging the Northeast of the",
            "            │ United States.",
            "            │",
            "            ● Resume                                                                    1h01m left",
        ]
    );
    assert_eq!(footer(&state).trim(), "enter play   esc back   d downloads   ? keys");
    assert_eq!(state.update(key(KeyCode::Enter)), [Effect::PlayTitle(video("tt0063350"))]);
}

#[test]
fn a_series_lists_its_seasons_and_episodes_and_where_you_stopped() {
    let mut state = opened((100, 30));
    assert_eq!(state.update(key(KeyCode::Char('l'))), [Effect::Title { ask: 1, id: title_id("tt9999901") }]);
    assert_eq!(list_lines(&state)[..2], ["     series │ Harbor Lights", "            │ looking up the title"]);
    state.update(Msg::Title { ask: 1, result: Ok(Box::new(harbor_page())) });
    assert_eq!(
        list_lines(&state)[..20],
        [
            "     series │ Harbor Lights",
            "            │ 2019 · 2 seasons · 44 min · Horror, Thriller · rated 8.1",
            "            │ A keeper and her daughter tend the last manned lighthouse on the coast.",
            "            │",
            "            ● Resume S01E03  Third Light                                                  22m left",
            "            │",
            "   season 1 │  1  First Light                                                              watched",
            "            │  2  Low Tide                                                                 watched",
            "            │  3  Third Light                                                   22m left · up next",
            "            │  4  Fog Bank",
            "            │",
            "   season 2 │  1  Return",
            "            │  2  Undertow                                                              airs in 5d",
            "            │",
            "   specials │  1  Lighthouse Diaries",
            "            │",
            "            │",
            "            │",
            "            │",
            "            │",
        ]
    );
    press(&mut state, "jj");
    assert_eq!(state.update(key(KeyCode::Enter)), [Effect::PlayTitle(video("tt9999901:1:2"))]);
    state.update(Msg::TitlePlayed(Ok((
        "player-stub".into(),
        Box::new(played("Harbor Lights S01E02", "tt9999901:1:2", None)),
    ))));
    press(&mut state, "jjjj");
    assert_eq!(state.pick(), Some(Pick::Episode(Episode { season: 2, episode: 2 })));
    assert_eq!(state.update(key(KeyCode::Enter)), [], "an episode yet to air does not play");
    assert!(footer(&state).starts_with("              S02E02 has not aired yet."), "{}", footer(&state));
    press(&mut state, "g");
    assert_eq!(state.pick(), Some(Pick::Next));
    assert_eq!(state.update(key(KeyCode::Char('P'))), [Effect::PlayTitle(video("tt9999901"))]);
}

#[test]
fn a_finished_film_and_a_caught_up_series_play_from_the_start() {
    let mut state = film((100, 30));
    state.update(Msg::Title { ask: 1, result: Ok(Box::new(night_page(Some(5750.0)))) });
    assert_eq!(list_lines(&state)[6], "            ● Play");
    let mut caught_up = harbor_page();
    caught_up.next = None;
    let mut state = series((100, 30));
    state.update(Msg::Title { ask: 1, result: Ok(Box::new(caught_up)) });
    assert_eq!(list_lines(&state)[4], "            ● Play S01E01  First Light");
}

#[test]
fn esc_steps_back_from_a_title_to_the_search_to_the_home_lists() {
    let mut state = film((100, 30));
    assert_eq!(state.update(key(KeyCode::Esc)), [Effect::Home { ask: 2 }]);
    assert_eq!(state.pick(), Some(Pick::Found(title_id("tt0063350"))));
    state.update(key(KeyCode::Esc));
    assert_eq!((state.titles.found.is_none(), state.titles.query.text.as_str()), (true, ""));
    assert_eq!(state.pick(), Some(Pick::Continue(video("tt9999901:1:3"))));
    state.update(key(KeyCode::Esc));
    assert!(state.titles.typing, "the last esc goes to the search box");
    state.update(key(KeyCode::Esc));
    assert!(!state.titles.typing, "esc in the search box goes back to the list");
}

#[test]
fn the_downloads_are_one_key_away_and_t_comes_back() {
    let mut state = opened((100, 30));
    press(&mut state, "d");
    assert_eq!((state.page, cursor_name(&state)), (Page::Downloads, "wikipedia-en-2026-09-text".into()));
    assert!(footer(&state).starts_with("              Press t for films and shows."), "{}", footer(&state));
    assert_eq!(press(&mut state, "t"), [Effect::Home { ask: 2 }]);
    assert_eq!((state.page, state.toast.is_none()), (Page::Titles, true), "the downloads page's toast stays there");
    assert_eq!(state.pick(), Some(Pick::Continue(video("tt9999901:1:3"))), "the cursor stays where it was");
}

#[test]
fn with_no_downloads_esc_in_the_empty_torrent_search_comes_back() {
    let mut state = titles_app((100, 30));
    state.update(Msg::Watch(Event::Snapshot { jobs: Vec::new() }));
    state.update(Msg::Home { ask: 1, result: Ok(home()) });
    press(&mut state, "d");
    assert_eq!(state.page, Page::Downloads);
    assert!(footer(&state).starts_with("              Press esc for films and shows."), "{}", footer(&state));
    press(&mut state, "arch");
    state.update(key(KeyCode::Esc));
    assert_eq!((state.page, state.query.text.as_str()), (Page::Downloads, ""), "the first esc clears the words");
    assert_eq!(state.update(key(KeyCode::Esc)), [Effect::Home { ask: 2 }]);
    assert_eq!(state.page, Page::Titles);
}

#[test]
fn the_update_key_shows_only_where_keys_are_not_typed() {
    let mut state = opened((100, 30));
    state.update(Msg::NewerVersion("0.5.1".into()));
    state.toast = None;
    assert!(footer(&state).starts_with("              torrential 0.5.1 available · U update"), "{}", footer(&state));
    press(&mut state, "/");
    state.toast = None;
    assert!(footer(&state).starts_with("              torrential 0.5.1 available "), "{}", footer(&state));
    assert!(!footer(&state).contains("U update"), "{}", footer(&state));
}

#[test]
fn typing_d_into_the_search_box_types_it() {
    let mut state = empty((100, 30));
    press(&mut state, "dune");
    assert_eq!((state.page, state.titles.query.text.as_str()), (Page::Titles, "dune"));
}

#[test]
fn the_home_lists_are_asked_again_when_the_engine_comes_back() {
    let mut state = opened((100, 30));
    let effects = state.update(Msg::Watch(Event::Snapshot { jobs: jobs() }));
    assert!(!effects.iter().any(|e| matches!(e, Effect::Home { .. })), "the engine was there all along: {effects:?}");
    state.update(Msg::WatchLost(Error::new(ErrorCode::DaemonUnavailable, "the engine closed the connection")));
    let effects = state.update(Msg::Watch(Event::Snapshot { jobs: jobs() }));
    assert!(effects.contains(&Effect::Home { ask: 2 }), "{effects:?}");
}

#[test]
fn an_empty_home_says_what_to_do_and_an_absent_engine_says_so() {
    let state = empty((100, 30));
    assert_eq!(
        list_lines(&state)[..2],
        [
            "            │ Search for a film or a show to watch.",
            "            │ It plays in your player while it downloads."
        ]
    );
    assert!(state.titles.typing, "with nothing listed, keys type into the search box");
    assert_eq!(footer(&state).trim(), "enter search   f1 keys");
    let mut gone = titles_app((100, 30));
    gone.update(Msg::WatchLost(Error::new(ErrorCode::DaemonUnavailable, "connection refused")));
    assert_eq!(
        list_lines(&gone)[..2],
        [
            "     engine │ Cannot reach the engine.",
            "            │ Films and shows are unavailable until the engine is running.",
        ]
    );
    press(&mut gone, "nosferatu");
    gone.update(key(KeyCode::Enter));
    gone.update(Msg::FoundTitles { search: 1, result: Ok(Vec::new()) });
    assert_eq!(
        list_lines(&gone)[..2],
        ["     titles │ No films or shows named \"nosferatu\".", "            │ Try fewer or different words."]
    );
}

#[test]
fn a_late_answer_to_an_old_search_or_title_changes_nothing() {
    let mut state = searching((100, 30));
    press(&mut state, "/sintel");
    state.update(key(KeyCode::Enter));
    state.update(Msg::FoundTitles { search: 1, result: Ok(found()) });
    assert_eq!(state.titles.found.as_ref().map(|f| &f.titles), Some(&Load::Loading));
    state.update(Msg::FoundTitles { search: 2, result: Ok(vec![sintel()]) });
    assert_eq!(state.update(key(KeyCode::Enter)), [Effect::Title { ask: 1, id: title_id("tt1727587") }]);
    state.update(key(KeyCode::Esc));
    assert_eq!(state.update(key(KeyCode::Enter)), [Effect::Title { ask: 2, id: title_id("tt1727587") }]);
    state.update(Msg::Title { ask: 1, result: Ok(Box::new(night_page(None))) });
    assert_eq!(state.titles.open.as_ref().map(|o| &o.page), Some(&Load::Loading));
}

#[test]
fn a_late_answer_to_an_older_home_or_title_request_changes_nothing() {
    let mut state = opened((100, 30));
    press(&mut state, "d");
    assert_eq!(press(&mut state, "t"), [Effect::Home { ask: 2 }]);
    press(&mut state, "d");
    assert_eq!(press(&mut state, "t"), [Effect::Home { ask: 3 }]);
    state.update(Msg::Home { ask: 3, result: Ok(home()) });
    let late = Error::new(ErrorCode::DaemonUnavailable, "the engine did not respond in time");
    state.update(Msg::Home { ask: 2, result: Err(late) });
    assert_eq!(state.titles.home, Load::Ready(home()));

    assert_eq!(press(&mut state, "l"), [Effect::Title { ask: 1, id: title_id("tt9999901") }]);
    state.update(key(KeyCode::Esc));
    assert_eq!(press(&mut state, "l"), [Effect::Title { ask: 2, id: title_id("tt9999901") }]);
    let late = Error::new(ErrorCode::FetchFailed, "cannot look up tt9999901 on Cinemeta");
    state.update(Msg::Title { ask: 1, result: Err(late) });
    assert_eq!(state.titles.open.as_ref().map(|o| &o.page), Some(&Load::Loading));
    state.update(Msg::Title { ask: 2, result: Ok(Box::new(harbor_page())) });
    assert_eq!(state.titles.open.as_ref().map(|o| &o.page), Some(&Load::Ready(harbor_page())));
}

#[test]
fn esc_clears_the_search_box_then_leaves_it_even_when_nothing_was_found() {
    let mut state = opened((100, 30));
    press(&mut state, "/nosferatu");
    state.update(key(KeyCode::Enter));
    state.update(Msg::FoundTitles { search: 1, result: Ok(Vec::new()) });
    press(&mut state, "/");
    assert_eq!((state.titles.typing, state.titles.query.text.as_str()), (true, "nosferatu"));
    state.update(key(KeyCode::Esc));
    assert_eq!((state.titles.typing, state.titles.query.text.as_str()), (true, ""), "the first esc clears the words");
    state.update(key(KeyCode::Esc));
    assert_eq!(
        (state.titles.typing, state.titles.found.is_none()),
        (false, true),
        "the next esc steps back to the home lists"
    );
    assert_eq!(state.pick(), Some(Pick::Continue(video("tt9999901:1:3"))));

    let mut nothing = empty((100, 30));
    press(&mut nothing, "dune");
    nothing.update(key(KeyCode::Esc));
    nothing.update(key(KeyCode::Esc));
    assert_eq!((nothing.titles.typing, nothing.titles.query.text.as_str()), (false, ""));
    assert_eq!(footer(&nothing).trim(), "/ search   d downloads   ? keys");
}

#[test]
fn a_play_that_fails_says_why_and_what_to_do_above_the_list_until_esc() {
    let mut state = opened((100, 30));
    state.update(key(KeyCode::Enter));
    let url = "http://127.0.0.1:47919/4c1e9a5d3b8e2f1c4a5d3b8e2f1c4a5d3b8e2f1c/0/night_of_the_living_dead.mp4";
    let no_player =
        Error::new(ErrorCode::NotFound, "no media player found (looked for IINA, mpv, and VLC)").with_hint(format!(
            "open {url} in any player, or set player: in /Users/ada/.config/torrential/config.yaml to the player's \
             absolute path"
        ));
    state.update(Msg::TitlePlayed(Err(no_player)));
    assert_eq!(
        list_lines(&state)[..7],
        [
            "       play │ Playback failed: no media player found (looked for IINA, mpv, and VLC)",
            "            │ open http://127.0.0.1:47919/4c1e9a5d3b8e2f1c4a5d3b8e2f1c4a5d3b8e2f1c/0/night_of_the_",
            "            │ living_dead.mp4 in any player, or set player: in",
            "            │ /Users/ada/.config/torrential/config.yaml to the player's absolute path",
            "            │",
            "   continue ● Harbor Lights  S01E03 · Third Light                                         22m left",
            "            │ Night of the Living Dead (1968)                                           1h01m left",
        ]
    );
    assert!(footer(&state).starts_with("              Playback failed: no media player found"), "{}", footer(&state));

    press(&mut state, "jjj");
    assert_eq!(state.update(key(KeyCode::Char('P'))), [Effect::PlayTitle(video("tt1727587"))]);
    assert_eq!(
        list_lines(&state)[0],
        "   continue │ Harbor Lights  S01E03 · Third Light                                         22m left"
    );
    let tried = vec![
        Tried { release: result_id("4c1e9a"), reason: Skip::NoData { waited_s: 30 } },
        Tried { release: result_id("b7f20d"), reason: Skip::NoVideo },
    ];
    let none = Error::new(ErrorCode::NoSource, "no source of Sintel (2010) started")
        .with_hint("search the torrents for it by name")
        .with_details(Details::Tried(tried));
    state.update(Msg::TitlePlayed(Err(none)));
    assert_eq!(
        list_lines(&state)[..6],
        [
            "       play │ Playback failed: no source of Sintel (2010) started",
            "            │ Skipped 4c1e9a: no data arrived in 30s",
            "            │ Skipped b7f20d: none of its files is the video",
            "            │ search the torrents for it by name",
            "            │",
            "   continue │ Harbor Lights  S01E03 · Third Light                                         22m left",
        ]
    );
    state.update(key(KeyCode::Esc));
    assert_eq!(
        list_lines(&state)[0],
        "   continue │ Harbor Lights  S01E03 · Third Light                                         22m left"
    );
    assert_eq!((state.titles.typing, state.pick()), (false, Some(Pick::Saved(title_id("tt1727587")))));
}

#[test]
fn a_skipped_source_wraps_on_a_narrow_terminal_like_the_message_and_the_hint() {
    let mut state = opened((47, 19));
    state.update(key(KeyCode::Enter));
    let tried = vec![
        Tried { release: result_id("4c1e9a"), reason: Skip::Unplayable { container: "mkv".into() } },
        Tried { release: result_id("b7f20d"), reason: Skip::NoVideo },
    ];
    let none = Error::new(ErrorCode::NoSource, "no source of Sintel (2010) started")
        .with_hint("search the torrents for it by name")
        .with_details(Details::Tried(tried));
    state.update(Msg::TitlePlayed(Err(none)));
    assert_eq!(
        list_lines(&state)[..8],
        [
            "│ play",
            "│ Playback failed: no source of Sintel (2010)",
            "│ started",
            "│ Skipped 4c1e9a: its .mkv file needs ffmpeg,",
            "│ which is not installed",
            "│ Skipped b7f20d: none of its files is the",
            "│ video",
            "│ search the torrents for it by name",
        ]
    );
}

#[test]
fn a_long_series_scrolls_with_the_cursor_and_keeps_the_season_in_the_margin() {
    let mut state = series((100, 12));
    press(&mut state, "jjjjj");
    assert_eq!(
        list_lines(&state),
        [
            "   season 1 ↑  3  Third Light                                                   22m left · up next",
            "            │  4  Fog Bank",
            "            │",
            "   season 2 ●  1  Return",
            "            │  2  Undertow                                                              airs in 5d",
            "            ↓",
        ]
    );
    press(&mut state, "G");
    let lines = list_lines(&state);
    assert_eq!(lines[0], "   season 1 ↑  4  Fog Bank");
    assert_eq!(lines[5], "   specials ●  1  Lighthouse Diaries");
}

#[test]
fn titles_screens() {
    snapshots("titles_home", opened);
    snapshots("titles_found", searching);
    snapshots("titles_film", film);
    snapshots("titles_series", series);
    snapshots("titles_help", |size| {
        let mut state = opened(size);
        press(&mut state, "?");
        state
    });
}
