use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use torrential_core::protocol::{GetOutcome, GetReply};
use torrential_core::{Preferences, Profile};

use super::*;
use crate::theme::{self, ColorMode};
use crate::{layout, view};

const SIZES: [(u16, u16); 5] = [(80, 24), (100, 30), (160, 45), (47, 40), (47, 19)];

fn shot(state: &State) -> String {
    let (w, h) = state.size;
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal
        .draw(|frame| {
            view::render(frame.buffer_mut(), state, ColorMode::TrueColor);
        })
        .unwrap();
    terminal.backend().to_string()
}

fn snapshots(name: &str, scenario: impl Fn((u16, u16)) -> State) {
    for size in SIZES {
        insta::assert_snapshot!(format!("{name}_{}x{}", size.0, size.1), shot(&scenario(size)));
    }
}

fn idle(size: (u16, u16)) -> State {
    let mut state = with_downloads(size);
    state.update(Msg::Files { item: Item::Download(id("6f2c81")), result: Ok(wikipedia_files()) });
    state
}

fn streaming(size: (u16, u16)) -> State {
    let mut state = with_downloads(size);
    press(&mut state, "/");
    press(&mut state, "night of the living dead");
    state.update(key(KeyCode::Enter));
    let (partial, pending) = partial();
    state.update(Msg::SearchUpdate { search: 1, result: partial, pending });
    state
}

fn file_tree(size: (u16, u16)) -> State {
    let mut state = searched(size);
    press(&mut state, "j");
    state.update(key(KeyCode::Enter));
    state.update(Msg::Files { item: Item::Result(result_id("b7f20d")), result: Ok(dvd_files()) });
    press(&mut state, "jjjj jj ");
    state
}

fn download(size: (u16, u16)) -> State {
    let mut state = searched(size);
    press(&mut state, "d");
    state.update(Msg::Files { item: Item::Download(id("6f2c81")), result: Ok(wikipedia_files()) });
    state
}

fn ranking(size: (u16, u16)) -> State {
    let mut state = searched(size);
    press(&mut state, "Sjjlll");
    state
}

#[test]
fn ranking_screens() {
    snapshots("ranking", ranking);
}

#[test]
fn idle_screens() {
    snapshots("idle", idle);
}

#[test]
fn a_newer_version_shows_in_the_footer() {
    snapshots("newer_version", |size| {
        let mut state = idle(size);
        state.update(Msg::NewerVersion("0.1.1".into()));
        state
    });
}

#[test]
fn streaming_screens() {
    snapshots("streaming", streaming);
}

#[test]
fn file_tree_screens() {
    snapshots("file_tree", file_tree);
}

#[test]
fn download_screens() {
    snapshots("download", download);
}

#[test]
fn download_files_screens() {
    snapshots("download_files", |size| {
        let mut state = with_downloads(size);
        press(&mut state, "jj");
        state.update(Msg::Files { item: Item::Download(id("91fd3b")), result: Ok(stackexchange_files()) });
        state.update(key(KeyCode::Enter));
        press(&mut state, "jjjjjjjjjjjjjj");
        state
    });
}

#[test]
fn help_screens() {
    snapshots("help", |size| {
        let mut state = searched(size);
        press(&mut state, "?");
        state
    });
}

#[test]
fn a_download_says_stalled_only_after_a_quiet_spell() {
    let mut state = with_downloads((100, 30));
    let wikipedia = |rate: u64| {
        let mut job = jobs()[0].clone();
        job.progress.down_rate = rate;
        job.progress.eta = None;
        Msg::Watch(Event::Job { job: Box::new(job) })
    };
    state.update(wikipedia(600));
    let trickled = state.now;
    assert!(line(&render(&state), 4).ends_with("22%"), "bytes arrive without an ETA: {}", line(&render(&state), 4));
    later(&mut state, 20_000);
    state.update(wikipedia(0));
    assert!(line(&render(&state), 4).ends_with("22%"), "quiet for 20 s: {}", line(&render(&state), 4));
    assert_eq!(state.next_stall(), Some(trickled + Duration::from_secs(30)));
    later(&mut state, 10_000);
    state.update(Msg::Tick);
    assert!(line(&render(&state), 4).ends_with("22%       stalled"), "{}", line(&render(&state), 4));
    assert_eq!(state.next_stall(), None);
}

#[test]
fn a_failed_download_says_why_in_a_few_words_at_every_width() {
    let failed = |code: ErrorCode, size: (u16, u16)| {
        let mut state = with_downloads(size);
        let mut job = jobs()[1].clone();
        job.status = JobStatus::Error { error: Error::new(code, "the engine stopped it") };
        state.update(Msg::Watch(Event::Job { job: Box::new(job) }));
        let buf = render(&state);
        let (x, y) = find(&buf, "openwebtext-2019");
        let why = (x..size.0).find(|&x| buf[(x, y)].fg == theme::ERROR).map(|x| buf[(x, y)].symbol().to_string());
        (line(&buf, y), why)
    };
    let (row, ink) = failed(ErrorCode::DiskFull, (80, 24));
    assert!(row.ends_with("━━╸─────   36%     disk full"), "{row}");
    assert_eq!(ink.as_deref(), Some("d"), "the reason takes the error ink");
    let (row, _) = failed(ErrorCode::NoSource, (100, 30));
    assert!(row.ends_with("36%     no source"), "{row}");
    let (row, _) = failed(ErrorCode::NoSource, (47, 19));
    assert_eq!(row, "│ openwebtext-2019              no source  36%", "the reason stands in for the bar");
    let (row, _) = failed(ErrorCode::MetadataTimeout, (40, 19));
    assert_eq!(row, "│ openwebtext-2019     no metadata  36%", "wider than the bar it stands in for");
}

fn wikipedia_with(status: JobStatus, map: Option<&str>) -> (State, Buffer, u16) {
    let mut state = with_downloads((100, 30));
    let mut job = jobs()[0].clone();
    job.status = status;
    job.progress.piece_map = map.map(|m| m.parse().unwrap());
    state.update(Msg::Watch(Event::Job { job: Box::new(job) }));
    let buf = render(&state);
    let (_, y) = find(&buf, "wikipedia-en-2026-09-text");
    (state, buf, y)
}

const SCATTERED: &str = concat!(
    "8-8-", "--44", "0--0", "-01-", "8--7", "12--", "----", "6666", "-22-", "3-4-", "----", "8888", "0-0-", "--0-",
    "-0--", "4444",
);

const ONE_FILE: &str = concat!("----------------------------------------", "84026", "-------------------");

#[test]
fn a_download_getting_bytes_shows_where_its_pieces_sit() {
    let (state, buf, y) = wikipedia_with(JobStatus::Downloading, Some(SCATTERED));
    assert_eq!(
        line(&buf, y),
        "    getting ● wikipedia-en-2026-09-text                       █▄▁▁▇▂▆▆▂▄██▁▁▄▄   22%      30m left"
    );
    let (x, _) = find(&buf, "█▄▁▁▇");
    let plain = render_in(&state, ColorMode::NoColor);
    assert!(plain[(x + 2, y)].modifier.contains(Modifier::DIM), "nothing has arrived: a faint floor");
    assert!(!plain[(x + 3, y)].modifier.contains(Modifier::DIM), "an eighth has arrived: a full-ink floor");
}

#[test]
fn a_gauge_spreads_a_few_wanted_slices_across_every_cell() {
    let (_, buf, y) = wikipedia_with(JobStatus::Downloading, Some(ONE_FILE));
    assert_eq!(
        line(&buf, y),
        "    getting ● wikipedia-en-2026-09-text                       ████▄▄▄▁▁▁▂▂▂▆▆▆   22%      30m left"
    );
    let (_, buf, y) = wikipedia_with(JobStatus::Downloading, Some(&"-".repeat(64)));
    assert!(line(&buf, y).contains(" ━━━╸──────────── "), "nothing wanted: {}", line(&buf, y));
}

#[test]
fn a_stalled_download_draws_the_rule_until_bytes_arrive_again() {
    let mut state = with_downloads((100, 30));
    state.update(Msg::Files { item: Item::Download(id("6f2c81")), result: Ok(wikipedia_files()) });
    let wikipedia = |rate: u64| {
        let mut job = jobs()[0].clone();
        job.progress.down_rate = rate;
        job.progress.eta = None;
        Msg::Watch(Event::Job { job: Box::new(job) })
    };
    let gauge = "    getting ● wikipedia-en-2026-09-text                       ▃▂▁▄▂▂▁▃▂▁▃▂▂▂▁▁   22%";
    let file_ink = |buf: &Buffer, x: u16| buf[(x, find(buf, "s-multistream.xml.bz2").1)].fg;
    state.update(wikipedia(600));
    let buf = render(&state);
    let (x, _) = find(&buf, "▃▂▁▄");
    assert_eq!((line(&buf, 4), file_ink(&buf, x)), (gauge.to_string(), theme::ACTION), "bytes arrive");
    later(&mut state, 20_000);
    state.update(wikipedia(0));
    assert_eq!(line(&render(&state), 4), gauge, "quiet for 20 s");
    later(&mut state, 10_000);
    state.update(Msg::Tick);
    let buf = render(&state);
    let stalled = "    getting ● wikipedia-en-2026-09-text                       ━━━╸────────────   22%       stalled";
    assert_eq!(line(&buf, 4), stalled);
    assert_eq!((buf[(x, 4)].fg, file_ink(&buf, x)), (theme::PENCIL, theme::PENCIL), "no bytes: pencil, not sage");
    state.update(wikipedia(600));
    assert_eq!(line(&render(&state), 4), gauge, "bytes again");
}

#[test]
fn a_download_keeps_the_rule_until_it_has_a_map_and_while_it_is_not_getting_bytes() {
    let (_, buf, y) = wikipedia_with(JobStatus::Downloading, None);
    assert!(line(&buf, y).contains(" ━━━╸──────────── "), "no map yet: {}", line(&buf, y));
    let (_, buf, y) = wikipedia_with(JobStatus::Paused, Some(SCATTERED));
    assert!(line(&buf, y).contains(" ━━━╸──────────── "), "paused: {}", line(&buf, y));
}

#[test]
fn a_long_query_is_cut_so_failed_sources_stay_in_view() {
    let mut state = with_downloads((80, 24));
    press(&mut state, "/night of the living dead 1968 1080p bluray x264");
    state.update(key(KeyCode::Enter));
    state.update(Msg::SearchDone { search: 1, result: Ok(result(night()[2..3].to_vec(), final_sources())) });
    let buf = render(&state);
    assert_eq!(line(&buf, 2), "     search │ night of the living dead 1968 1080p bluray…   1 found · 2 failed");
    let (x, y) = find(&buf, "2 failed");
    assert_eq!(buf[(x, y)].fg, theme::ERROR);
    press(&mut state, "/");
    let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));
    let caret = view::render(&mut buf, &state, ColorMode::TrueColor);
    assert_eq!(line(&buf, 2), "     search ●  of the living dead 1968 1080p bluray x264    1 found · 2 failed");
    assert_eq!(caret, Some(ratatui::layout::Position::new(56, 2)));
}

#[test]
fn a_toast_never_pushes_out_the_first_hint() {
    let started = "Started Night of the Living Dead (1968) [DVD]";
    let mut state = searched((100, 30));
    press(&mut state, "d");
    state.say(crate::state::Tone::Success, started);
    let footer = line(&render(&state), 29);
    assert_eq!(footer, format!("{:14}{started}{:8}space pause   x remove   o open", "", ""));
    state.update(Msg::Resize(60, 16));
    let footer = line(&render(&state), 15);
    assert_eq!(footer, format!("{:14}Started Night of the Living…     space pause", ""));
}

#[test]
fn every_file_of_a_download_is_one_key_away() {
    let mut state = with_downloads((80, 24));
    press(&mut state, "jj");
    state.update(Msg::Files { item: Item::Download(id("91fd3b")), result: Ok(stackexchange_files()) });
    let screen = shot(&state);
    assert!(screen.contains("and 15 more files · press enter to list them"), "{screen}");
    state.update(key(KeyCode::Enter));
    press(&mut state, "G");
    let buf = render(&state);
    let (_, y) = find(&buf, "part20.7z");
    assert_eq!(buf[(MARK, y)].symbol(), "●");
    assert!(line(&buf, y).ends_with("100%       100 MiB"), "{}", line(&buf, y));
    assert_eq!(buf[(MARK, y - 12)].symbol(), "↑", "more files above");
    assert!(line(&buf, 23).ends_with("space resume   esc back   o open   ? keys"), "{}", line(&buf, 23));
    state.update(key(KeyCode::Esc));
    assert_eq!(state.focus, crate::state::Focus::List);
}

#[test]
fn one_peer_reads_as_one_peer() {
    let mut state = with_downloads((100, 30));
    let mut job = jobs()[0].clone();
    job.progress.peers = 1;
    state.update(Msg::Watch(Event::Job { job: Box::new(job.clone()) }));
    let buf = render(&state);
    assert_eq!(line(&buf, 6), "            │   5.3 GiB of 23.0 GiB · 10.0 MiB/s · 1 peer");
    job.status = JobStatus::Metadata;
    state.update(Msg::Watch(Event::Job { job: Box::new(job) }));
    let buf = render(&state);
    assert!(line(&buf, 4).ends_with("1 peer"), "{}", line(&buf, 4));
    assert_eq!(line(&buf, 6), "            │   fetching the file list from peers · 1 peer");
}

#[test]
fn one_source_reads_as_one_source() {
    let mut state = with_downloads((100, 30));
    press(&mut state, "/night");
    state.update(key(KeyCode::Enter));
    let (partial, _) = partial();
    let one = SearchResult { sources: partial.sources[..1].to_vec(), ..partial };
    state.update(Msg::SearchUpdate { search: 1, result: one, pending: Vec::new() });
    assert!(line(&render(&state), 2).ends_with("searching · 1 of 1 source"), "{}", line(&render(&state), 2));
}

#[test]
fn every_footer_keeps_its_way_out_and_its_keys_hint_at_every_width() {
    fn newer(size: (u16, u16)) -> State {
        let mut state = idle(size);
        state.update(Msg::NewerVersion("0.1.1".into()));
        state
    }
    let typing = |size| {
        let mut state = newer(size);
        press(&mut state, "/");
        state
    };
    type Footer = (fn((u16, u16)) -> State, &'static [&'static str], &'static str);
    let footers: [Footer; 3] = [
        (newer, &["U update"], "? keys"),
        (typing, &["0.1.1 is out"], "f1 keys"),
        (file_tree, &["esc back"], "? keys"),
    ];
    for w in 40..=160 {
        for (scenario, kept, last) in footers {
            let footer = line(&render(&scenario((w, 24))), 23);
            assert!(footer.ends_with(last), "{w} columns: {footer}");
            for word in kept {
                assert!(footer.contains(word), "{w} columns: {footer}");
            }
        }
    }
    assert!(line(&render(&newer((47, 19))), 18).ends_with("U update to 0.1.1       space pause   ? keys"));
    let footer = line(&render(&file_tree((47, 19))), 18);
    assert!(footer.ends_with("space pick   esc back   ? keys"), "the lower pairs go first: {footer}");
}

#[test]
fn a_typing_user_sees_that_f1_opens_the_keys() {
    let mut state = with_downloads((100, 30));
    press(&mut state, "/");
    assert!(line(&render(&state), 29).ends_with("enter search   ↓ list   f1 keys"), "{}", line(&render(&state), 29));
    press(&mut state, "night");
    state.update(key(KeyCode::F(1)));
    assert!(shot(&state).contains("     always │ ? f1 keys"), "{}", shot(&state));
}

#[test]
fn a_cut_name_keeps_its_end() {
    let screen = shot(&searched((80, 24)));
    assert!(screen.contains("Night of…(1968) [DVD]"), "{screen}");
    assert!(screen.contains("Night.of.the…BluRay.x264"), "{screen}");
}

#[test]
fn a_row_shows_the_resolution_and_its_details_spell_out_the_quality() {
    let mut state = searched((100, 30));
    press(&mut state, "jj");
    let buf = render(&state);
    let (_, y) = find(&buf, "BluRay.x264");
    assert!(line(&buf, y).ends_with("BluRay.x264      trusted   1080p   7.8 GiB    214    3y"), "{}", line(&buf, y));
    assert_eq!(line(&buf, y + 1), "            │   knaben · trusted uploader · 1080p h264");
    let (_, y) = find(&buf, "CAM.x264");
    assert!(
        line(&buf, y).ends_with("CAM.x264        cam               1.4 GiB     41   2mo"),
        "a blank, never the codec, when no resolution"
    );
    let (_, y) = find(&buf, "(1968) colorized");
    assert!(line(&buf, y).ends_with("colorized     pd                734 MiB    web    6y"), "{}", line(&buf, y));
}

fn heads_row(state: &State) -> String {
    let buf = render(state);
    line(&buf, find(&buf, "found │").1)
}

#[test]
fn a_film_search_says_what_it_ranked_for_in_as_many_parts_as_fit() {
    assert_eq!(
        heads_row(&searched((160, 45))),
        "      found │ ranked for this Mac: 1080p screen, shows HDR, plays HEVC, arm64, English           size  seeds   age"
    );
    assert_eq!(
        heads_row(&searched((100, 30))),
        "      found │ ranked for this Mac: 1080p screen, shows HDR, plays HEVC, arm64    size  seeds   age"
    );
    assert_eq!(
        heads_row(&searched((80, 24))),
        "      found │ ranked for this Mac: 1080p screen, shows HDR   size  seeds   age"
    );
    assert_eq!(heads_row(&searched((60, 16))), "      found │ ranked for this Mac        size  seeds   age");
    let mut state = searched((100, 30));
    press(&mut state, "s");
    assert!(heads_row(&state).contains("found │ most seeds first"), "another order ranks for nothing");
}

#[test]
fn a_search_ranks_for_what_its_results_depend_on() {
    let found = |releases: Vec<Release>| {
        let mut state = with_downloads((100, 30));
        press(&mut state, "/ubuntu");
        state.update(key(KeyCode::Enter));
        state.update(Msg::SearchDone { search: 1, result: Ok(result(releases, final_sources())) });
        heads_row(&state)
    };
    let iso = |name| Release {
        category: Some(Category::Software),
        ..release("6a1f00", name, "knaben", &[], Some(950), 6_100 * MIB, 400)
    };
    assert_eq!(
        found(vec![iso("ubuntu-24.04.1-desktop-amd64.iso")]),
        "      found │ ranked for this Mac: arm64                                         size  seeds   age",
        "software that names an architecture"
    );
    assert_eq!(
        found(vec![iso("Ubuntu Linux Bible, 10th Edition")]),
        "      found │ best match first                                                   size  seeds   age"
    );
}

fn searched_for(size: (u16, u16), profile: Profile) -> State {
    let mut state = with_downloads(size);
    press(&mut state, "/night");
    state.update(key(KeyCode::Enter));
    let mut found = result(night(), final_sources());
    found.query.profile = profile;
    state.update(Msg::SearchDone { search: 1, result: Ok(found) });
    state
}

#[test]
fn a_search_before_detection_says_so_without_cutting_the_words() {
    let pending = || Profile { detected: None, ..Profile::default() };
    assert_eq!(
        heads_row(&searched_for((80, 24), pending())),
        "      found │ ranked for this machine, not detected yet      size  seeds   age"
    );
    assert_eq!(
        heads_row(&searched_for((60, 16), pending())),
        "      found │ best match first           size  seeds   age",
        "never \"ranked for this machine\""
    );
}

#[test]
fn a_search_with_overrides_names_them_or_only_the_machine() {
    let with = |preferences| Profile { detected: Some(mac()), preferences, room: None };
    let tv = || with(Preferences { screen: Some(torrential_core::Screen::Tv4k), ..Preferences::default() });
    assert_eq!(
        heads_row(&searched_for((160, 45), tv())),
        "      found │ ranked for this Mac: 2160p screen, arm64, English · overrides: screen tv-4k        size  seeds   age"
    );
    assert_eq!(
        heads_row(&searched_for((100, 30), tv())),
        "      found │ ranked for this Mac: 2160p screen · overrides: screen tv-4k        size  seeds   age"
    );
    assert_eq!(
        heads_row(&searched_for((80, 24), tv())),
        "      found │ ranked for this Mac · overrides: screen tv-4k  size  seeds   age"
    );
    let h264 = with(Preferences { playback: Some(torrential_core::Playback::H264), ..Preferences::default() });
    assert_eq!(
        heads_row(&searched_for((80, 24), h264)),
        "      found │ ranked for this Mac                            size  seeds   age",
        "not \"1080p screen, no HDR\" without the H.264 that the override set"
    );
}

fn panel_rows(state: &State) -> Vec<String> {
    let buf = render(state);
    let (_, y) = find(&buf, "ranking │");
    (y..y + 4).map(|y| line(&buf, y)).collect()
}

#[test]
fn the_ranking_panel_shows_what_was_detected_and_takes_the_one_mark() {
    let mut state = searched((80, 24));
    press(&mut state, "S");
    assert_eq!(
        panel_rows(&state),
        [
            "    ranking │ for this Mac: 1080p screen, shows HDR, plays HEVC, arm64",
            "            ● screen    [auto] phone  laptop  tv  tv-4k         detected 1080p",
            "            │ max size  [none] 2 GiB  8 GiB  20 GiB",
            "            │ playback  [auto] hevc-hdr  hevc  h264          detected hevc-hdr",
        ]
    );
    let buf = render(&state);
    let marks: Vec<u16> = (0..24).filter(|&y| buf[(MARK, y)].symbol() == "●").collect();
    assert_eq!(marks, [find(&buf, "screen    [auto]").1], "the panel has the keys, not the result under the cursor");
    assert!(!shot(&state).contains("archive.org · public domain"), "the cursor's details close");
    assert!(line(&buf, 23).ends_with("enter save   esc close   ← → choose"), "{}", line(&buf, 23));
    press(&mut state, "l");
    let plain = render_in(&state, ColorMode::NoColor);
    let (x, y) = find(&plain, "[phone]");
    assert!(plain[(x, y)].modifier.contains(Modifier::BOLD), "the pick is bold without color");
    assert!(plain[(x - 5, y)].modifier.contains(Modifier::DIM), "the other choices are faint");
    assert!(line(&plain, y).contains("screen     auto [phone] laptop  tv"), "the words stay put: {}", line(&plain, y));
}

#[test]
fn the_panel_previews_what_search_would_rank_for() {
    let mut state = searched((100, 30));
    press(&mut state, "Sllll");
    assert_eq!(
        panel_rows(&state),
        [
            "    ranking │ for this Mac: 2160p screen, arm64, English",
            "            ● screen     auto  phone  laptop  tv [tv-4k]                            detected 1080p",
            "            │ max size  [none] 2 GiB  8 GiB  20 GiB",
            "            │ playback  [auto] hevc-hdr  hevc  h264                                 plays anything",
        ],
        "a TV's player is not this Mac's, so nothing limits playback"
    );
    assert!(heads_row(&state).contains("ranked for this Mac: 1080p screen"), "the list keeps its order until a save");
    assert!(heads_row(&state).contains(", plays HEVC (saved)"), "and says it ranks for the saved picks");
    press(&mut state, "jl");
    assert_eq!(
        panel_rows(&state)[2],
        "            ● max size   none [2 GiB] 8 GiB  20 GiB                       refuses larger downloads"
    );
}

#[test]
fn before_any_search_the_panel_shows_the_machine_detected_at_start() {
    let mut state = with_downloads((100, 30));
    press(&mut state, "S");
    assert_eq!(panel_rows(&state)[0], "    ranking │ for this machine, not detected yet");
    assert!(panel_rows(&state)[1].ends_with("not detected yet"), "{}", panel_rows(&state)[1]);
    state.update(Msg::Machine(mac()));
    assert_eq!(
        panel_rows(&state)[0],
        "    ranking │ for this Mac: 1080p screen, shows HDR, plays HEVC, arm64, English"
    );
    assert!(panel_rows(&state)[1].ends_with("detected 1080p"));
}

#[test]
fn the_panel_opens_on_the_saved_overrides_and_fits_60_columns() {
    let five = Preferences {
        screen: Some(torrential_core::Screen::Laptop),
        max_size: Some(5_000_000_000),
        playback: Some(torrential_core::Playback::H264),
    };
    let mut state = searched_for((60, 16), Profile { detected: Some(mac()), preferences: five, room: None });
    press(&mut state, "S");
    assert_eq!(
        panel_rows(&state),
        [
            "    ranking │ for this Mac: 1080p screen, no HDR",
            "            ● screen     auto  phone [laptop] tv  tv-4k",
            "            │ max size   none  2 GiB [5 GB] 8 GiB  20 GiB",
            "            │ playback   auto  hevc-hdr  hevc [h264]",
        ],
        "the notes at the ends drop where they do not fit"
    );
}

#[test]
fn help_lists_the_ranking_key() {
    let mut state = searched((100, 30));
    press(&mut state, "?");
    assert!(shot(&state).contains("S ranking"), "{}", shot(&state));
    press(&mut state, "S");
    assert_eq!((state.help, state.ranking.is_some()), (false, true), "help gives way to the panel");
}

#[test]
fn a_file_list_error_sits_three_cells_after_the_facts() {
    let mut state = searched((100, 30));
    let error = Error::new(ErrorCode::MetadataTimeout, "no peer sent the file list");
    state.update(Msg::Files { item: Item::Result(result_id("4c1e9a")), result: Err(error) });
    let buf = render(&state);
    let (_, y) = find(&buf, "no peer sent");
    assert_eq!(line(&buf, y), "            │   archive.org · public domain   no peer sent the file list");
}

#[test]
fn a_narrow_window_keeps_the_room_for_names() {
    let buf = render(&searched((60, 16)));
    let (_, y) = find(&buf, "3.4 GiB");
    assert_eq!(line(&buf, y), "            │ Night…[DVD]  pd         3.4 GiB    web   12y");
}

#[test]
fn a_search_without_qualities_keeps_the_room_for_names() {
    let mut state = with_downloads((100, 30));
    press(&mut state, "/");
    press(&mut state, "ubuntu");
    state.update(key(KeyCode::Enter));
    let iso =
        release("6a1f00", "ubuntu-24.04.1-desktop-amd64.iso", "knaben", &[Flag::Official], Some(950), 6_100 * MIB, 400);
    state.update(Msg::SearchDone { search: 1, result: Ok(result(vec![iso], final_sources())) });
    let buf = render(&state);
    let (_, y) = find(&buf, "ubuntu-24.04.1");
    assert!(
        line(&buf, y).ends_with("amd64.iso                     official   6.0 GiB    950    1y"),
        "{}",
        line(&buf, y)
    );
}

#[test]
fn a_small_window_asks_to_grow() {
    assert!(shot(&app((80, 9))).contains("Make the window at least 30×10."));
    let narrow = shot(&app((24, 12)));
    assert!(narrow.contains("Make the window") && narrow.contains("at least 30×10."), "{narrow}");
}

#[test]
fn every_screen_draws_at_every_size_from_the_smallest_up() {
    let scenarios: [fn((u16, u16)) -> State; 7] = [
        idle,
        streaming,
        file_tree,
        download,
        ranking,
        |size| {
            let mut state = searched(size);
            press(&mut state, "?");
            state
        },
        |size| {
            let mut state = with_downloads(size);
            press(&mut state, "jjx");
            state
        },
    ];
    for w in layout::MIN_SIZE.0..=61 {
        for h in [layout::MIN_SIZE.1, 12, 16, 19, 24] {
            for scenario in scenarios {
                shot(&scenario((w, h)));
            }
        }
    }
}

fn render(state: &State) -> Buffer {
    render_in(state, ColorMode::TrueColor)
}

fn render_in(state: &State, mode: ColorMode) -> Buffer {
    let (w, h) = state.size;
    let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
    view::render(&mut buf, state, mode);
    buf
}

fn find(buf: &Buffer, text: &str) -> (u16, u16) {
    let w = buf.area.width;
    for y in 0..buf.area.height {
        let line: String = (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect();
        if let Some(byte) = line.find(text) {
            return (line[..byte].chars().count() as u16, y);
        }
    }
    panic!("{text:?} is not on screen");
}

fn line(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()
}

const MARK: u16 = 12;

#[test]
fn the_selected_row_is_bright_and_bold_with_the_mark_beside_it() {
    let mut state = searched((100, 30));
    press(&mut state, "j");
    let buf = render(&state);
    let (x, y) = find(&buf, "Night of the Living Dead (1968) [DVD]");
    assert_eq!((buf[(x, y)].fg, buf[(x, y)].modifier.contains(Modifier::BOLD)), (theme::INK, true));
    assert_eq!((buf[(MARK, y)].symbol(), buf[(MARK, y)].fg), ("●", theme::ACTION));
    let (x, y) = find(&buf, "Night.of.the…Dead.1968.1080p");
    assert_eq!((buf[(x, y)].fg, buf[(MARK, y)].symbol()), (theme::TEXT, "│"));
}

#[test]
fn the_mark_shows_where_keys_go() {
    let mut state = searched((100, 30));
    press(&mut state, "/");
    let buf = render(&state);
    let marks: Vec<u16> = (0..30).filter(|&y| buf[(MARK, y)].symbol() == "●").collect();
    assert_eq!(marks, [crate::layout::SEARCH], "typing: the mark is on the search line");
    let (x, y) = find(&buf, "Night of the Living Dead (1968)");
    assert_eq!(buf[(x, y)].fg, theme::TEXT, "no row is selected while typing");
}

#[test]
fn the_margin_labels_each_group_once() {
    let state = searched((100, 30));
    let buf = render(&state);
    let margins: Vec<String> =
        (0..30).map(|y| line(&buf, y).chars().take(11).collect::<String>().trim().to_string()).collect();
    let labels: Vec<&str> = margins.iter().map(String::as_str).filter(|m| !m.is_empty()).collect();
    assert_eq!(labels, ["torrential", "search", "getting", "error", "paused", "done", "found"]);
    let (x, y) = find(&buf, "error │");
    assert_eq!((x, buf[(x, y)].fg), (6, theme::ERROR));
}

#[test]
fn errors_and_warnings_take_the_error_ink() {
    let state = searched((100, 30));
    let buf = render(&state);
    let (x, y) = find(&buf, "apibay and nyaa failed");
    assert_eq!(buf[(x, y)].fg, theme::ERROR);
    let (x, y) = find(&buf, "cam ");
    assert_eq!(buf[(x, y)].fg, theme::ERROR);
    let (_, y) = find(&buf, "720p WEB");
    let zero = (0..100).rev().find(|&x| buf[(x, y)].symbol() == "0").unwrap();
    assert_eq!(buf[(zero, y)].fg, theme::ERROR, "no seeds is in the red");
}

#[test]
fn remove_asks_in_the_margin_and_says_the_files_stay() {
    let mut state = searched((100, 30));
    press(&mut state, "djjx");
    let buf = render(&state);
    let (x, y) = find(&buf, "remove?");
    assert_eq!((x, buf[(x, y)].fg), (4, theme::ERROR));
    assert!(line(&buf, y).contains("● stackexchange"), "{}", line(&buf, y));
    assert!(line(&buf, y + 1).contains("Its files stay in ~/Downloads/torrential."), "{}", line(&buf, y + 1));
    assert!(line(&buf, y + 1).ends_with("enter removes it   esc keeps it"));
    assert_eq!(line(&buf, 29), "", "the footer does not repeat the keys");
}

#[test]
fn a_result_in_downloads_offers_the_way_there() {
    let mut state = searched((100, 30));
    let job = torrential_core::Job { id: id("4c1e9a"), ..jobs()[0].clone() };
    let reply = GetReply { outcome: GetOutcome::Started, job, plan: None, previous_selection: None };
    state.update(Msg::Started { release: result_id("4c1e9a"), result: Ok(Box::new(reply)) });
    state.toast = None;
    let buf = render(&state);
    assert!(line(&buf, 29).ends_with("d downloads   s sort   ? keys"), "{}", line(&buf, 29));
    assert!(shot(&state).contains("in downloads · press d to see it"));
}

#[test]
fn without_color_the_mark_the_ticks_and_the_words_still_carry_meaning() {
    let mut state = searched((100, 30));
    press(&mut state, "j");
    state.update(key(KeyCode::Enter));
    state.update(Msg::Files { item: Item::Result(result_id("b7f20d")), result: Ok(dvd_files()) });
    press(&mut state, "jj ");
    let buf = render_in(&state, ColorMode::NoColor);
    assert!(buf.content.iter().all(|c| c.fg == Color::Reset && c.bg == Color::Reset), "no color at all");
    let (x, y) = find(&buf, "Night.asr.srt");
    assert_eq!(buf[(MARK, y)].symbol(), "●");
    assert!(buf[(x, y)].modifier.contains(Modifier::BOLD));
    assert!(line(&buf, y).contains("· Night.asr.srt"), "space left it out: {}", line(&buf, y));
    assert!(line(&buf, y + 1).contains("✓ Night.gif"), "{}", line(&buf, y + 1));
    let (x, y) = find(&buf, "ranked for this Mac");
    assert!(buf[(x, y)].modifier.contains(Modifier::DIM), "pencil turns faint");
    let screen = shot(&state);
    assert!(screen.contains("1 executable left out"), "{screen}");
}

#[test]
fn low_disk_space_is_said_in_words() {
    let mut state = searched((100, 30));
    press(&mut state, "j");
    state.update(key(KeyCode::Enter));
    state.update(Msg::Files { item: Item::Result(result_id("b7f20d")), result: Ok(dvd_files()) });
    state.update(Msg::FreeSpace(Some(3 << 30)));
    let buf = render_in(&state, ColorMode::NoColor);
    assert!(line(&buf, 0).ends_with("↓ 10.0 MiB/s   low on space · 3.0 GiB free"), "{}", line(&buf, 0));
    let (_, y) = find(&buf, "11 of 12 files");
    assert!(line(&buf, y).contains("11 of 12 files · 398 MiB short of space"), "{}", line(&buf, y));
    state.update(Msg::FreeSpace(Some(40 << 30)));
    let buf = render_in(&state, ColorMode::NoColor);
    assert!(line(&buf, 0).ends_with("     ↓ 10.0 MiB/s"), "nothing follows the rate: {}", line(&buf, 0));
    assert!(line(&buf, y).contains("11 of 12 files · 40.0 GiB free"), "{}", line(&buf, y));
}

#[test]
fn an_unreachable_engine_says_so_with_its_hint() {
    let mut state = app((100, 30));
    let error = Error::new(ErrorCode::DaemonUnavailable, "the engine did not start within 10s")
        .with_hint("see ~/.local/state/torrential/engine.log");
    state.update(Msg::WatchLost(error));
    let screen = shot(&state);
    assert!(screen.contains("engine unreachable"), "{screen}");
    assert!(screen.contains("engine │ Cannot reach the engine."), "{screen}");
    assert!(screen.contains("see ~/.local/state/torrential/engine.log"), "{screen}");
    assert!(screen.contains("Cannot reach the engine: the engine did not start within 10s"), "{screen}");
}

#[test]
fn a_frame_drawn_before_the_resize_message_arrives_still_fits() {
    let mut state = searched((160, 45));
    press(&mut state, "d");
    state.update(Msg::Files { item: Item::Download(id("6f2c81")), result: Ok(wikipedia_files()) });
    let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));
    view::render(&mut buf, &state, ColorMode::TrueColor);
    let mut tiny = Buffer::empty(Rect::new(0, 0, 61, 16));
    view::render(&mut tiny, &state, ColorMode::TrueColor);
    assert_eq!(find(&buf, "torrential"), (1, 0));
}
