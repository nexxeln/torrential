use crossterm::event::KeyCode;
use torrential_core::{Playback, Preferences, Screen};

use crate::state::Edit;

use super::*;

const TV_4K: Preferences = Preferences { screen: Some(Screen::Tv4k), max_size: None, playback: None };
const LAPTOP: Preferences =
    Preferences { screen: Some(Screen::Laptop), max_size: Some(8_000_000_000), playback: Some(Playback::H264) };

fn searched_with(preferences: Preferences) -> State {
    let mut state = with_downloads((100, 30));
    press(&mut state, "/night of the living dead");
    state.update(key(KeyCode::Enter));
    let mut found = result(night(), final_sources());
    found.query.profile.preferences = preferences;
    state.update(Msg::SearchDone { search: 1, result: Ok(found) });
    state
}

fn night_query() -> SearchQuery {
    SearchQuery { limit: 50, ..SearchQuery::new("night of the living dead") }
}

fn saved(edit: &Effect, current: Preferences) -> Msg {
    let Effect::SavePreferences(edit) = edit else { panic!("expected a save, got {edit:?}") };
    Msg::Saved { edit: *edit, result: Ok(edit.apply(current)) }
}

fn edit(from: Preferences, to: Preferences) -> Effect {
    Effect::SavePreferences(Edit { from, to })
}

fn right(state: &mut State, times: usize) {
    for _ in 0..times {
        state.update(key(KeyCode::Right));
    }
}

#[test]
fn nothing_but_shift_s_opens_the_panel() {
    let mut state = with_downloads((100, 30));
    state.update(Msg::Preferences(Preferences::default()));
    state.update(Msg::Machine(mac()));
    press(&mut state, "/night of the living dead");
    state.update(key(KeyCode::Enter));
    let mut unset = result(night(), final_sources());
    unset.query.profile.detected = None;
    state.update(Msg::SearchDone { search: 1, result: Ok(unset) });
    later(&mut state, 5_000);
    state.update(Msg::Tick);
    let nothing = Edit { from: Preferences::default(), to: Preferences::default() };
    state.update(Msg::Saved { edit: nothing, result: Ok(Preferences::default()) });
    assert_eq!(state.ranking, None, "a film search with nothing saved or detected asks nothing");

    let mut codes: Vec<KeyCode> = (' '..='~').filter(|c| *c != 'S').map(KeyCode::Char).collect();
    codes.extend([KeyCode::Enter, KeyCode::Esc, KeyCode::Tab, KeyCode::Up, KeyCode::Down, KeyCode::F(1)]);
    for code in codes {
        let mut state = searched((100, 30));
        state.update(key(code));
        assert_eq!(state.ranking, None, "{code:?}");
    }
    let mut typing = searched((100, 30));
    press(&mut typing, "/S");
    assert_eq!((typing.ranking, typing.query.text.as_str()), (None, "night of the living deadS"), "S is a letter here");
    let mut state = searched((100, 30));
    press(&mut state, "S");
    assert_eq!(state.ranking.map(|r| r.picks), Some(Preferences::default()));
}

#[test]
fn enter_saves_a_tv_4k_screen_and_searches_again() {
    let mut state = searched((100, 30));
    press(&mut state, "S");
    right(&mut state, 4);
    let save = state.update(key(KeyCode::Enter));
    assert_eq!(save, [edit(Preferences::default(), TV_4K)]);
    assert_eq!(state.ranking, None);
    let effects = state.update(saved(&save[0], Preferences::default()));
    assert_eq!(effects, [Effect::Search { search: 2, query: night_query() }]);
    assert_eq!((state.toast.unwrap().text.as_str(), state.preferences), ("Saved screen tv-4k.", Some(TV_4K)));
}

#[test]
fn esc_closes_the_panel_and_saves_nothing() {
    let mut state = searched_with(TV_4K);
    press(&mut state, "S");
    state.update(key(KeyCode::Left));
    state.update(key(KeyCode::Down));
    right(&mut state, 2);
    assert_eq!(state.update(key(KeyCode::Esc)), []);
    assert_eq!((state.ranking, state.preferences), (None, Some(TV_4K)));
    assert_eq!(cursor_name(&state), "Night of the Living Dead (1968)", "the list has the keys again");
    press(&mut state, "S");
    assert_eq!(state.ranking.map(|r| r.picks), Some(TV_4K), "the next open starts on what is saved");
    assert_eq!(press(&mut state, "S"), [], "S closes it too");
    assert_eq!(state.ranking, None);
}

#[test]
fn enter_with_nothing_changed_closes_without_a_save() {
    let mut state = searched_with(LAPTOP);
    press(&mut state, "S");
    state.update(key(KeyCode::Right));
    state.update(key(KeyCode::Left));
    assert_eq!(state.update(key(KeyCode::Enter)), []);
    assert_eq!(state.ranking, None);
}

#[test]
fn each_row_starts_on_its_override_and_auto_clears_it() {
    let mut state = searched_with(LAPTOP);
    press(&mut state, "S");
    let ranking = state.ranking.unwrap();
    let picked = |row: crate::state::Row| row.value(ranking.choices(row)[ranking.picked(row)]);
    assert_eq!(crate::state::ROWS.map(picked), ["laptop", "8 GB", "h264"]);
    press(&mut state, "hhhhh");
    let cleared = Preferences { screen: None, ..LAPTOP };
    let save = state.update(key(KeyCode::Enter));
    assert_eq!(save, [edit(LAPTOP, cleared)]);
    let effects = state.update(saved(&save[0], LAPTOP));
    assert_eq!(state.preferences, Some(cleared));
    assert_eq!(effects, [Effect::Search { search: 2, query: night_query() }]);
    assert_eq!(state.toast.unwrap().text, "Saved screen auto.");
}

#[test]
fn a_saved_cap_the_panel_does_not_offer_is_still_a_choice() {
    let five = Preferences { max_size: Some(5_000_000_000), ..Preferences::default() };
    let mut state = searched_with(five);
    press(&mut state, "Sj");
    let ranking = state.ranking.unwrap();
    let labels: Vec<String> = ranking.choices(ranking.row).into_iter().map(|c| ranking.row.value(c)).collect();
    assert_eq!(labels, ["none", "2 GiB", "5 GB", "8 GiB", "20 GiB"]);
    assert_eq!(ranking.picked(ranking.row), 2);
    press(&mut state, "l");
    let eight = Preferences { max_size: Some(8 << 30), ..Preferences::default() };
    assert_eq!(state.update(key(KeyCode::Enter)), [edit(five, eight)]);
}

#[test]
fn the_keys_stop_at_the_ends_of_rows_and_choices() {
    let mut state = searched((100, 30));
    press(&mut state, "S");
    press(&mut state, "kkhh");
    assert_eq!(state.ranking.map(|r| (r.row, r.picks)), Some((crate::state::Row::Screen, Preferences::default())));
    press(&mut state, "jjjjllllll");
    let h264 = Preferences { playback: Some(Playback::H264), ..Preferences::default() };
    assert_eq!(state.ranking.map(|r| (r.row, r.picks)), Some((crate::state::Row::Playback, h264)));
}

#[test]
fn shift_s_before_any_search_opens_on_the_overrides_read_at_start() {
    let mut state = with_downloads((100, 30));
    state.update(Msg::Preferences(LAPTOP));
    press(&mut state, "S");
    assert_eq!(state.ranking.map(|r| r.picks), Some(LAPTOP));
    press(&mut state, "l");
    let tv = Preferences { screen: Some(Screen::Tv), ..LAPTOP };
    let save = state.update(key(KeyCode::Enter));
    assert_eq!(save, [edit(LAPTOP, tv)]);
    assert_eq!(state.update(saved(&save[0], LAPTOP)), [], "no search to run again");
}

#[test]
fn the_overrides_read_at_start_never_replace_newer_ones() {
    let mut state = with_downloads((100, 30));
    let made = Edit { from: Preferences::default(), to: LAPTOP };
    state.update(Msg::Saved { edit: made, result: Ok(LAPTOP) });
    state.update(Msg::Preferences(Preferences::default()));
    press(&mut state, "S");
    assert_eq!(state.ranking.map(|r| r.picks), Some(LAPTOP));
}

#[test]
fn a_save_while_a_search_runs_starts_it_again() {
    let mut state = with_downloads((100, 30));
    press(&mut state, "/night of the living dead");
    state.update(key(KeyCode::Enter));
    press(&mut state, "S");
    right(&mut state, 4);
    let save = state.update(key(KeyCode::Enter));
    let effects = state.update(saved(&save[0], Preferences::default()));
    assert_eq!(effects, [Effect::Search { search: 2, query: night_query() }], "the running search had none");
}

#[test]
fn a_failed_save_says_why_and_searches_nothing() {
    let mut state = searched((100, 30));
    let error = Error::new(ErrorCode::StorageError, "cannot write /Users/ada/.config/torrential/config.yaml");
    let tried = Edit { from: Preferences::default(), to: TV_4K };
    assert_eq!(state.update(Msg::Saved { edit: tried, result: Err(error) }), []);
    assert_eq!(state.toast.unwrap().text, "Saving failed: cannot write /Users/ada/.config/torrential/config.yaml");
    assert_eq!(state.preferences, Some(Preferences::default()));
}

#[test]
fn the_saved_toast_names_the_changes_that_fit_beside_the_first_hint() {
    let every =
        Preferences { screen: Some(Screen::Tv4k), max_size: Some(20_000_000_000), playback: Some(Playback::H264) };
    let toast = |size: (u16, u16), to: Preferences| {
        let mut state = app(size);
        let made = Edit { from: Preferences::default(), to };
        state.update(Msg::Saved { edit: made, result: Ok(to) });
        state.toast.unwrap().text
    };
    assert_eq!(toast((100, 30), every), "Saved screen tv-4k, max size 20 GB, playback h264.");
    assert_eq!(toast((80, 24), every), "Saved screen tv-4k, max size 20 GB and 1 more.");
    assert_eq!(toast((70, 20), every), "Saved screen tv-4k and 2 more.");
    assert_eq!(toast((60, 16), every), "Saved 3 changes.");
    assert_eq!(toast((60, 16), TV_4K), "Saved screen tv-4k.");
}

#[test]
fn a_search_reports_the_machine_and_the_overrides() {
    let state = searched_with(LAPTOP);
    assert_eq!((state.preferences, state.machine), (Some(LAPTOP), Some(mac())));
    let mut state = with_downloads((100, 30));
    state.update(Msg::Machine(mac()));
    press(&mut state, "/night");
    state.update(key(KeyCode::Enter));
    let mut pending = result(night(), final_sources());
    pending.query.profile.detected = None;
    state.update(Msg::SearchDone { search: 1, result: Ok(pending) });
    assert_eq!(state.machine, Some(mac()), "a search before detection keeps the machine detected at start");
}

#[test]
fn a_save_changes_only_the_rows_it_moved() {
    let mut state = with_downloads((100, 30));
    press(&mut state, "S");
    press(&mut state, "jjl");
    let save = state.update(key(KeyCode::Enter));
    let hevc_hdr = Preferences { playback: Some(Playback::HevcHdr), ..Preferences::default() };
    assert_eq!(save, [edit(Preferences::default(), hevc_hdr)], "the panel opened before the overrides were read");
    let Effect::SavePreferences(made) = save[0] else { unreachable!() };
    let on_disk = Preferences { playback: None, ..LAPTOP };
    assert_eq!(
        made.apply(on_disk),
        Preferences { playback: Some(Playback::HevcHdr), ..LAPTOP },
        "laptop and 8 GB stay"
    );
}

#[test]
fn a_save_keeps_a_typed_filter_and_runs_the_search_again() {
    let mut state = searched((100, 30));
    press(&mut state, "/");
    press(&mut state, " 1080p");
    state.update(key(KeyCode::Down));
    assert_eq!(cursor_name(&state), "Night.of.the.Living.Dead.1968.1080p.BluRay.x264");
    press(&mut state, "Sllll");
    let save = state.update(key(KeyCode::Enter));
    let effects = state.update(saved(&save[0], Preferences::default()));
    assert_eq!(effects, [Effect::Search { search: 2, query: night_query() }]);
    assert_eq!(state.query.text, "night of the living dead 1080p", "the filter stays");
}

#[test]
fn a_reply_to_the_search_before_the_save_is_dropped() {
    let mut state = searched((100, 30));
    press(&mut state, "Sllll");
    let save = state.update(key(KeyCode::Enter));
    state.update(saved(&save[0], Preferences::default()));
    state.update(Msg::SearchDone { search: 1, result: Ok(result(night(), final_sources())) });
    assert_eq!(state.preferences, Some(TV_4K), "the old search ranked with no overrides");
    let mut found = result(night(), final_sources());
    found.query.profile.preferences = TV_4K;
    state.update(Msg::SearchDone { search: 2, result: Ok(found) });
    assert_eq!(state.preferences, Some(TV_4K));
}
