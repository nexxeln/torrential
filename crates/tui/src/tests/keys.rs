use crossterm::event::KeyCode;
use torrential_core::protocol::{Change, GetOutcome, GetReply};

use super::*;
use crate::state::{Files, Focus, JobAction, Upgrade};

#[test]
fn opens_on_the_downloads_when_there_are_some() {
    let state = with_downloads((100, 30));
    assert_eq!((state.focus, cursor_name(&state)), (Focus::List, "wikipedia-en-2026-09-text".into()));
    let mut empty = app((100, 30));
    empty.update(Msg::Watch(Event::Snapshot { jobs: Vec::new() }));
    assert_eq!((empty.focus, empty.cursor), (Focus::Query, None));
}

#[test]
fn downloads_list_active_then_failed_then_paused_then_done() {
    let state = with_downloads((100, 30));
    let names: Vec<String> = state.downloads.iter().map(|j| j.name.clone().unwrap()).collect();
    assert_eq!(
        names,
        [
            "wikipedia-en-2026-09-text",
            "openwebtext-2019",
            "stackexchange",
            "big_buck_bunny_1080p_h264.mov",
            "archlinux-2026.09.01-x86_64.iso"
        ]
    );
}

#[test]
fn paused_and_done_downloads_fold_while_the_cursor_is_in_the_results() {
    let mut state = searched((100, 30));
    assert_eq!(cursor_name(&state), "Night of the Living Dead (1968)");
    assert_eq!(state.folded(), Some((1, 2)));
    assert_eq!(state.download_rows().len(), 2);
    state.update(key(KeyCode::Char('k')));
    assert_eq!(cursor_name(&state), "archlinux-2026.09.01-x86_64.iso");
    assert_eq!((state.folded(), state.download_rows().len()), (None, 5));
    press(&mut state, "jj");
    assert_eq!(cursor_name(&state), "Night of the Living Dead (1968) [DVD]");
}

#[test]
fn moving_keys_reach_every_item() {
    let mut state = searched((100, 30));
    press(&mut state, "g");
    assert_eq!(cursor_name(&state), "wikipedia-en-2026-09-text");
    press(&mut state, "G");
    assert_eq!(cursor_name(&state), "Night of the Living Dead 1968 720p WEB");
    press(&mut state, "r");
    state.update(key(KeyCode::PageDown));
    assert_eq!(
        cursor_name(&state),
        "Night of the Living Dead 1968 720p WEB",
        "every row fits, so a page reaches the end"
    );
    state.update(key(KeyCode::PageUp));
    assert_eq!(cursor_name(&state), "wikipedia-en-2026-09-text");
    press(&mut state, "d");
    assert_eq!(cursor_name(&state), "wikipedia-en-2026-09-text");
    press(&mut state, "r");
    assert_eq!(cursor_name(&state), "Night of the Living Dead (1968)");
    state.update(key(KeyCode::Tab));
    assert_eq!(state.focus, Focus::Query);
    state.update(key(KeyCode::Tab));
    assert_eq!((state.focus, cursor_name(&state)), (Focus::List, "Night of the Living Dead (1968)".into()));
    state.update(key(KeyCode::BackTab));
    assert_eq!(cursor_name(&state), "wikipedia-en-2026-09-text");
    state.update(key(KeyCode::BackTab));
    assert_eq!(state.focus, Focus::Query);
}

#[test]
fn h_collapses_the_selected_item_and_l_expands_it() {
    let mut state = with_downloads((100, 30));
    assert!(!state.collapsed);
    press(&mut state, "h");
    assert!(state.collapsed);
    press(&mut state, "l");
    assert!(!state.collapsed);
    press(&mut state, "hj");
    assert!(!state.collapsed, "moving on opens the next item");
}

#[test]
fn enter_opens_the_file_tree_where_space_and_a_pick_files() {
    let mut state = searched((100, 30));
    press(&mut state, "j");
    let effects = state.update(key(KeyCode::Enter));
    assert_eq!(effects, [Effect::Files(Item::Result(result_id("b7f20d")))]);
    assert_eq!(state.focus, Focus::Files);
    assert!(matches!(state.files.get(&result_id("b7f20d")), Some(Files::Loading { .. })));
    state.update(Msg::Files { item: Item::Result(result_id("b7f20d")), result: Ok(dvd_files()) });
    let tree = |state: &State| match state.files.get(&result_id("b7f20d")) {
        Some(Files::Ready(tree)) => tree.clone(),
        other => panic!("no tree: {other:?}"),
    };
    assert_eq!((tree(&state).picked_count(), tree(&state).risky_left_out()), (11, 1));
    press(&mut state, "jj ");
    assert_eq!(tree(&state).picked_count(), 10);
    press(&mut state, "a");
    assert_eq!(tree(&state).picked_count(), 0);
    let effects = state.update(key(KeyCode::Enter));
    assert_eq!(effects, []);
    assert_eq!(state.toast.as_ref().unwrap().text, "Nothing picked. Press space to pick a file.");
    press(&mut state, "j ");
    let effects = state.update(key(KeyCode::Enter));
    let files = Some(vec!["Night.gif".to_string()]);
    assert_eq!(effects, [Effect::Get { release: result_id("b7f20d"), files }]);
    assert_eq!((state.focus, cursor_name(&state)), (Focus::List, "Night of the Living Dead (1968) [DVD]".into()));
}

#[test]
fn h_and_l_fold_folders_in_the_tree() {
    let mut state = searched((100, 30));
    press(&mut state, "j");
    state.update(key(KeyCode::Enter));
    state.update(Msg::Files { item: Item::Result(result_id("b7f20d")), result: Ok(dvd_files()) });
    let visible = |state: &State| match state.files.get(&result_id("b7f20d")) {
        Some(Files::Ready(tree)) => tree.visible().len(),
        _ => 0,
    };
    assert_eq!(visible(&state), 14);
    press(&mut state, "h");
    assert_eq!(visible(&state), 1);
    state.update(key(KeyCode::Right));
    assert_eq!(visible(&state), 14);
}

#[test]
fn shift_d_without_a_file_list_fetches_one_and_downloads_the_safe_files() {
    let mut state = searched((100, 30));
    press(&mut state, "j");
    let effects = press(&mut state, "D");
    assert_eq!(effects, [Effect::Files(Item::Result(result_id("b7f20d")))]);
    assert_eq!(state.focus, Focus::List);
    let effects = state.update(Msg::Files { item: Item::Result(result_id("b7f20d")), result: Ok(dvd_files()) });
    let Some(Effect::Get { release, files: Some(files) }) = effects.first() else { panic!("{effects:?}") };
    assert_eq!(
        (release, files.len(), files.contains(&"extras/setup.exe".to_string())),
        (&result_id("b7f20d"), 11, false)
    );
}

#[test]
fn a_started_download_shows_at_once_and_keeps_the_cursor_on_the_result() {
    let mut state = searched((100, 30));
    let started = jobs()[0].clone();
    let job = torrential_core::Job { id: id("4c1e9a"), name: Some("night_of_the_living_dead".into()), ..started };
    let reply = GetReply { outcome: GetOutcome::Started, job, plan: None, previous_selection: None };
    state.update(Msg::Started { release: result_id("4c1e9a"), result: Ok(Box::new(reply)) });
    assert_eq!(state.downloads.len(), 6);
    assert_eq!(cursor_name(&state), "Night of the Living Dead (1968)");
    assert_eq!(state.toast.as_ref().unwrap().text, "Started Night of the Living Dead (1968)");
    assert_eq!(press(&mut state, "D"), [Effect::Files(Item::Result(result_id("4c1e9a")))]);
}

#[test]
fn an_existing_download_accepts_another_file_selection_including_every_file() {
    let mut state = searched((100, 30));
    let release = result_id("b7f20d");
    state.downloads.push(Job { id: id("b7f20d"), ..jobs()[3].clone() });
    press(&mut state, "j");
    state.update(key(KeyCode::Enter));
    let mut reply = dvd_files();
    reply.files.truncate(2);
    let paths: Vec<String> = reply.files.iter().map(|f| f.path.clone()).collect();
    state.update(Msg::Files { item: Item::Result(release), result: Ok(reply) });

    press(&mut state, "j ");
    assert_eq!(state.update(key(KeyCode::Enter)), [Effect::Get { release, files: Some(vec![paths[1].clone()]) }]);

    state.update(key(KeyCode::Enter));
    press(&mut state, " ");
    assert_eq!(
        state.update(key(KeyCode::Enter)),
        [Effect::Get { release, files: Some(paths) }],
        "picking every file must expand the existing download's selection"
    );
}

#[test]
fn the_esc_ladder_closes_help_then_leaves_the_tree_then_returns_to_the_query() {
    let mut state = searched((100, 30));
    press(&mut state, "j");
    state.update(key(KeyCode::Enter));
    state.update(Msg::Files { item: Item::Result(result_id("b7f20d")), result: Ok(dvd_files()) });
    press(&mut state, "?");
    assert_eq!((state.help, state.focus), (true, Focus::Files));
    state.update(key(KeyCode::Esc));
    assert_eq!((state.help, state.focus), (false, Focus::Files));
    state.update(key(KeyCode::Esc));
    assert_eq!(state.focus, Focus::List);
    state.update(key(KeyCode::Esc));
    assert_eq!(state.focus, Focus::Query);
    state.update(key(KeyCode::Esc));
    assert_eq!((state.focus, cursor_name(&state)), (Focus::List, "Night of the Living Dead (1968) [DVD]".into()));
}

#[test]
fn esc_in_an_empty_query_clears_it() {
    let mut state = app((100, 30));
    state.update(Msg::Watch(Event::Snapshot { jobs: Vec::new() }));
    press(&mut state, "arch");
    state.update(key(KeyCode::Esc));
    assert_eq!((state.query.text.as_str(), state.focus), ("", Focus::Query));
}

#[test]
fn typing_filters_loaded_results_and_enter_searches() {
    let mut state = searched((100, 30));
    press(&mut state, "/");
    press(&mut state, " colorized");
    assert_eq!(state.query.text, "night of the living dead colorized");
    let names: Vec<&str> = state.results().iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["Night of the Living Dead (1968) colorized"]);
    state.update(ctrl('w'));
    assert_eq!((state.query.text.as_str(), state.results().len()), ("night of the living dead ", 8));
    state.update(ctrl('u'));
    assert_eq!(state.query.text, "");
    press(&mut state, "q?/");
    assert_eq!((state.query.text.as_str(), state.quit, state.help), ("q?/", false, false));
    let effects = state.update(key(KeyCode::Enter));
    assert!(matches!(&effects[..], [Effect::Search { search: 2, query }] if query.text == "q?/"), "{effects:?}");
}

#[test]
fn streaming_results_follow_the_top_until_the_user_moves() {
    let mut state = with_downloads((80, 18));
    press(&mut state, "/");
    press(&mut state, "night of the living dead");
    let search = match &state.update(key(KeyCode::Enter))[..] {
        [Effect::Search { search, .. }] => *search,
        other => panic!("{other:?}"),
    };
    let (first, pending) = partial();
    state.update(Msg::SearchUpdate { search, result: first, pending });
    assert_eq!(cursor_name(&state), "Night of the Living Dead (1968)");
    press(&mut state, "j");
    assert_eq!(cursor_name(&state), "Night of the Living Dead (1968) [DVD]");
    let screen_row = |state: &State| {
        let placed = crate::layout::blocks(state, crate::layout::list_height(state));
        let (top, _) = crate::layout::span(&placed, state.cursor.unwrap()).unwrap();
        i32::from(top) - i32::from(state.scroll)
    };
    let row = screen_row(&state);
    let mut ranked = night();
    ranked.rotate_right(2);
    state.update(Msg::SearchUpdate { search, result: result(ranked, final_sources()), pending: vec!["nyaa".into()] });
    assert_eq!(cursor_name(&state), "Night of the Living Dead (1968) [DVD]");
    assert_eq!(screen_row(&state), row, "the selected row stays on the same screen line");
    let stale = Msg::SearchDone { search: search - 1, result: Ok(result(Vec::new(), Vec::new())) };
    state.update(stale);
    assert_eq!(state.results().len(), 8, "a replaced search's answer is ignored");
}

#[test]
fn space_pauses_and_the_row_waits_for_the_engine() {
    let mut state = with_downloads((100, 30));
    let effects = press(&mut state, " ");
    let wikipedia = id("6f2c81");
    assert_eq!(effects, [Effect::Job { id: wikipedia, action: JobAction::Pause }]);
    assert_eq!(state.pending.get(&wikipedia), Some(&JobAction::Pause));
    assert_eq!(press(&mut state, " "), [], "a second press waits for the first answer");
    let change = Change { id: wikipedia.to_hex(), name: None, result: "paused".into(), path: None };
    state.update(Msg::Changed { id: wikipedia, action: JobAction::Pause, result: Ok(vec![change]) });
    assert!(state.pending.is_empty());
    assert_eq!(state.toast.as_ref().unwrap().text, "Paused wikipedia-en-2026-09-text");
    let failed = Error::new(ErrorCode::NotFound, "no download matches 6f2c81");
    press(&mut state, "v");
    state.update(Msg::Changed { id: wikipedia, action: JobAction::Verify, result: Err(failed) });
    assert_eq!(state.toast.as_ref().unwrap().text, "Verify failed: no download matches 6f2c81");
}

#[test]
fn shift_p_plays_a_result_or_a_download_and_says_in_what() {
    let mut state = searched((100, 30));
    press(&mut state, "j");
    assert_eq!(press(&mut state, "P"), [Effect::Play(Item::Result(result_id("b7f20d")))]);
    assert_eq!(state.toast.as_ref().unwrap().text, "Starting the player…");
    state.update(Msg::Played(Ok("IINA".into())));
    assert_eq!(state.toast.as_ref().unwrap().text, "Playing in IINA");

    let mut state = with_downloads((100, 30));
    assert_eq!(press(&mut state, "P"), [Effect::Play(Item::Download(id("6f2c81")))]);
    let none = Error::new(ErrorCode::NoSelection, "wikipedia-en-2026-09-text has no video file");
    state.update(Msg::Played(Err(none)));
    assert_eq!(state.toast.as_ref().unwrap().text, "Playing failed: wikipedia-en-2026-09-text has no video file");
    assert_eq!(press(&mut state, "p"), [Effect::Job { id: id("6f2c81"), action: JobAction::Pause }]);
}

#[test]
fn remove_asks_first_and_keeps_files() {
    let mut state = with_downloads((100, 30));
    press(&mut state, "jj");
    let stackexchange = id("91fd3b");
    assert_eq!(press(&mut state, "x"), []);
    assert_eq!(state.confirm, Some(stackexchange));
    state.update(key(KeyCode::Esc));
    assert_eq!(state.confirm, None);
    press(&mut state, "x");
    let effects = state.update(key(KeyCode::Enter));
    assert_eq!(effects, [Effect::Job { id: stackexchange, action: JobAction::Remove }]);
    state.update(Msg::Changed { id: stackexchange, action: JobAction::Remove, result: Ok(Vec::new()) });
    assert_eq!(state.toast.as_ref().unwrap().text, "Removed from the list. Files stay in ~/Downloads/torrential.");
    assert_eq!((state.downloads.len(), cursor_name(&state)), (4, "big_buck_bunny_1080p_h264.mov".into()));
}

#[test]
fn selecting_a_download_asks_for_its_files_once_a_second_while_it_runs() {
    let mut state = app((100, 30));
    let effects = state.update(Msg::Watch(Event::Snapshot { jobs: jobs() }));
    assert_eq!(effects, [Effect::Files(Item::Download(id("6f2c81"))), Effect::FreeSpace]);
    let progress = Msg::Watch(Event::Job { job: Box::new(jobs()[0].clone()) });
    assert_eq!(state.update(progress), [Effect::FreeSpace]);
    later(&mut state, 1_100);
    let progress = Msg::Watch(Event::Job { job: Box::new(jobs()[0].clone()) });
    assert_eq!(state.update(progress), [Effect::Files(Item::Download(id("6f2c81")))]);
}

#[test]
fn ctrl_c_and_q_quit_but_q_types_in_the_query() {
    let mut state = with_downloads((100, 30));
    press(&mut state, "q");
    assert!(state.quit);
    let mut state = app((100, 30));
    press(&mut state, "q");
    assert!(!state.quit);
    state.update(ctrl('c'));
    assert!(state.quit);
}

#[test]
fn the_ellipsis_moves_on_its_own_beat_whatever_else_arrives() {
    let mut state = searched((100, 30));
    let start = state.started;
    later(&mut state, 240);
    assert_eq!((state.spinner(), state.next_frame()), ("   ", start + Duration::from_millis(250)));
    later(&mut state, 20);
    state.update(Msg::Watch(Event::Job { job: Box::new(jobs()[0].clone()) }));
    assert_eq!((state.spinner(), state.next_frame()), (".  ", start + Duration::from_millis(500)));
    later(&mut state, 500);
    assert_eq!(state.spinner(), "...");
}

#[test]
fn toasts_go_away_on_a_tick_after_their_time() {
    let mut state = with_downloads((100, 30));
    press(&mut state, "s");
    assert_eq!(state.toast.as_ref().unwrap().text, "Sorted by seeds");
    later(&mut state, 4_900);
    state.update(Msg::Tick);
    assert!(state.toast.is_some());
    later(&mut state, 200);
    state.update(Msg::Tick);
    assert!(state.toast.is_none());
}

#[test]
fn a_result_with_a_torrent_link_fetches_its_files_after_the_cursor_rests() {
    let mut state = searched((100, 30));
    let mut catalog = night();
    catalog[1].torrent_url = Some("https://archive.org/download/dvd/dvd_archive.torrent".into());
    let search = state.search.as_ref().unwrap().id;
    state.update(Msg::SearchDone { search, result: Ok(result(catalog, final_sources())) });
    press(&mut state, "j");
    later(&mut state, 200);
    assert_eq!(state.update(Msg::Tick), []);
    press(&mut state, "jk");
    later(&mut state, 200);
    assert_eq!(state.update(Msg::Tick), [], "moving on and back restarts the wait");
    later(&mut state, 150);
    assert_eq!(state.update(Msg::Tick), [Effect::Files(Item::Result(result_id("b7f20d")))]);
    press(&mut state, "k");
    later(&mut state, 400);
    assert_eq!(state.update(Msg::Tick), [], "a magnet-only result waits for enter");
}

#[test]
fn opening_the_top_result_stops_the_cursor_following_new_top_results() {
    let mut state = with_downloads((100, 30));
    press(&mut state, "/");
    press(&mut state, "night of the living dead");
    state.update(key(KeyCode::Enter));
    let (first, pending) = partial();
    state.update(Msg::SearchUpdate { search: 1, result: first, pending });
    state.update(key(KeyCode::Enter));
    assert_eq!(state.focus, Focus::Files);
    let mut ranked = night();
    ranked.rotate_right(1);
    state.update(Msg::SearchUpdate {
        search: 1,
        result: result(ranked, final_sources()),
        pending: vec!["nyaa".into()],
    });
    assert_eq!((state.focus, cursor_name(&state)), (Focus::Files, "Night of the Living Dead (1968)".into()));
}

#[test]
fn enter_in_a_failed_file_list_asks_again_instead_of_downloading() {
    let mut state = searched((100, 30));
    state.update(key(KeyCode::Enter));
    let timeout = Error::new(ErrorCode::MetadataTimeout, "no peer sent the file list for 4c1e9a in time");
    state.update(Msg::Files { item: Item::Result(result_id("4c1e9a")), result: Err(timeout) });
    let effects = state.update(key(KeyCode::Enter));
    assert_eq!(effects, [Effect::Files(Item::Result(result_id("4c1e9a")))]);
    assert_eq!(state.focus, Focus::Files);
    assert_eq!(state.update(key(KeyCode::Enter)), [], "enter while loading waits");
}

#[test]
fn a_requested_download_starts_even_after_the_cursor_moves_on() {
    let mut state = searched((100, 30));
    press(&mut state, "jD");
    press(&mut state, "j");
    let effects = state.update(Msg::Files { item: Item::Result(result_id("b7f20d")), result: Ok(dvd_files()) });
    assert!(matches!(&effects[..], [Effect::Get { release, .. }] if *release == result_id("b7f20d")), "{effects:?}");
}

#[test]
fn esc_that_closes_help_keeps_the_query() {
    let mut state = with_downloads((100, 30));
    press(&mut state, "?/night");
    state.update(key(KeyCode::Esc));
    assert_eq!((state.help, state.query.text.as_str()), (false, "night"));
}

#[test]
fn a_second_file_list_keeps_the_picks() {
    let mut state = searched((100, 30));
    press(&mut state, "j");
    state.update(key(KeyCode::Enter));
    let reply = || Msg::Files { item: Item::Result(result_id("b7f20d")), result: Ok(dvd_files()) };
    state.update(reply());
    press(&mut state, "a");
    state.update(reply());
    let Some(Files::Ready(tree)) = state.files.get(&result_id("b7f20d")) else { panic!("no tree") };
    assert_eq!(tree.picked_count(), 0);
}

#[test]
fn a_started_toast_names_the_download_after_the_results_change() {
    let mut state = searched((100, 30));
    let job =
        torrential_core::Job { id: id("4c1e9a"), name: Some("night_of_the_living_dead".into()), ..jobs()[0].clone() };
    press(&mut state, "/x");
    state.update(key(KeyCode::Enter));
    let reply = GetReply { outcome: GetOutcome::Started, job, plan: None, previous_selection: None };
    state.update(Msg::Started { release: result_id("4c1e9a"), result: Ok(Box::new(reply)) });
    assert_eq!(state.toast.as_ref().unwrap().text, "Started night_of_the_living_dead");
}

#[test]
fn inside_a_download_files_space_still_pauses_and_h_steps_back() {
    let mut state = with_downloads((100, 30));
    state.update(Msg::Files { item: Item::Download(id("6f2c81")), result: Ok(wikipedia_files()) });
    state.update(key(KeyCode::Enter));
    assert_eq!(state.focus, Focus::Files);
    press(&mut state, "j");
    assert_eq!(state.download_files[&id("6f2c81")].cursor, 1);
    assert_eq!(press(&mut state, " "), [Effect::Job { id: id("6f2c81"), action: JobAction::Pause }]);
    press(&mut state, "h");
    assert_eq!((state.focus, cursor_name(&state)), (Focus::List, "wikipedia-en-2026-09-text".into()));
}

#[test]
fn f1_opens_help_while_typing_where_question_mark_types() {
    let mut state = with_downloads((100, 30));
    press(&mut state, "/night?");
    assert_eq!((state.query.text.as_str(), state.help), ("night?", false));
    state.update(key(KeyCode::F(1)));
    assert_eq!((state.query.text.as_str(), state.help, state.focus), ("night?", true, Focus::Query));
    state.update(key(KeyCode::F(1)));
    assert!(!state.help);
}

#[test]
fn u_installs_a_newer_version_and_quits_to_restart_once_it_is_in_place() {
    let mut state = with_downloads((100, 30));
    assert_eq!(press(&mut state, "U"), [], "U does nothing until a newer version is known");
    state.update(Msg::NewerVersion("0.1.1".into()));

    let effects = press(&mut state, "U");
    let installing = (state.upgrade.clone(), state.spinning());
    state.update(Msg::Upgraded(Ok(Some("0.1.1".into()))));

    assert_eq!((effects, installing), (vec![Effect::Upgrade], (Some(Upgrade::Installing("0.1.1".into())), true)));
    assert_eq!((state.upgrade, state.quit), (Some(Upgrade::Installed), true));
}

#[test]
fn a_failed_update_says_why_and_u_can_try_again() {
    let mut state = with_downloads((100, 30));
    state.update(Msg::NewerVersion("0.1.1".into()));
    press(&mut state, "U");
    let failed = Error::new(ErrorCode::NetworkUnavailable, "cannot reach github.com");

    state.update(Msg::Upgraded(Err(failed)));

    assert_eq!(state.upgrade, Some(Upgrade::Available("0.1.1".into())));
    assert_eq!(state.toast.as_ref().map(|t| t.text.as_str()), Some("Updating failed: cannot reach github.com"));
    assert!(!state.quit);
    assert_eq!(press(&mut state, "U"), [Effect::Upgrade]);
}

#[test]
fn typing_u_in_the_search_box_types_it() {
    let mut state = app((100, 30));
    state.update(Msg::NewerVersion("0.1.1".into()));
    assert_eq!((press(&mut state, "U"), state.query.text.as_str()), (vec![], "U"));
}
