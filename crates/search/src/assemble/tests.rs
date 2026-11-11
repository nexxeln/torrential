use std::collections::BTreeSet;
use std::sync::Arc;

use torrential_core::error::Details;
use torrential_core::{
    Arch, Category, Display, ErrorCode, Flag, Language, Machine, Os, Playback, Preferences, Profile, Release,
    ReleaseId, Screen, SearchQuery, Sort, SourceOutcome, SourceReport,
};

use super::*;
use crate::observation::{Identity, Observation, Peers, infohash};

fn source(name: &str, authority: u16, lawful: bool, public: bool) -> Arc<SourceMeta> {
    Arc::new(SourceMeta { name: name.into(), authority, lawful, public })
}

fn archive() -> Arc<SourceMeta> {
    source("archive.org", 3, true, true)
}
fn knaben() -> Arc<SourceMeta> {
    source("knaben", 5, false, true)
}
fn apibay() -> Arc<SourceMeta> {
    source("apibay", 6, false, true)
}

fn row(hash: &str, name: &str) -> Observation {
    Observation::new(Identity::Hash { hash: infohash(hash).unwrap(), torrent_url: None }, name).unwrap()
}

fn with(mut o: Observation, seeders: Option<u32>, size: Option<u64>, published: Option<i64>) -> Observation {
    o.peers = Peers::new(seeders, None);
    o.size = size;
    o.published = published;
    o
}

fn fresh(source: &Arc<SourceMeta>, rows: Vec<Observation>) -> Run {
    Run { source: source.clone(), answer: Answer::Fresh(Arc::new(rows)), elapsed_ms: 40 }
}

fn failed(source: &Arc<SourceMeta>, failure: Failure) -> Run {
    Run { source: source.clone(), answer: Answer::Failed(failure), elapsed_ms: 6000 }
}

fn search(query: &SearchQuery, runs: &[Run]) -> SearchResult {
    assemble(query, query.profile.clone(), &Terms::parse(&query.text), runs, 1200).unwrap().0
}

fn names(result: &SearchResult) -> Vec<&str> {
    result.releases.iter().map(|r| r.name.as_str()).collect()
}

fn h(byte: u8) -> String {
    format!("{byte:02x}").repeat(20)
}

#[test]
fn merges_one_torrent_listed_by_two_sources() {
    let mut from_knaben = row(&h(0xaa), "Ubuntu 24.04 Desktop");
    from_knaben.peers = Some(Peers { seeders: 50, leechers: Some(5) });
    from_knaben.size = Some(100);
    from_knaben.published = Some(1000);
    from_knaben.category = Some(Category::Software);
    from_knaben.trackers = vec!["udp://a.example:1337/announce".into(), "udp://b.example:1337/announce".into()];
    let mut from_apibay = row(&h(0xaa).to_uppercase(), "Ubuntu 24.04 Desktop amd64 ISO").flag(Flag::Trusted, true);
    from_apibay.peers = Some(Peers { seeders: 80, leechers: None });
    from_apibay.size = Some(101);
    from_apibay.published = Some(900);
    from_apibay.trackers = vec!["udp://b.example:1337/announce".into(), "udp://c.example:1337/announce".into()];
    from_apibay.info_url = Some("https://thepiratebay.org/description.php?id=1".into());

    let result = search(
        &SearchQuery::new("ubuntu 24.04"),
        &[fresh(&apibay(), vec![from_apibay]), fresh(&knaben(), vec![from_knaben])],
    );

    let id = format!("{:02x}", 0xaa).repeat(20);
    assert_eq!(
        result.releases,
        [Release {
            id: ReleaseId::Torrent(id.parse().unwrap()),
            name: "Ubuntu 24.04 Desktop amd64 ISO".into(),
            size: Some(100),
            seeders: Some(80),
            leechers: None,
            published: Some(1000),
            category: Some(Category::Software),
            flags: BTreeSet::from([Flag::Trusted]),
            sources: vec!["knaben".into(), "apibay".into()],
            magnet: Some(format!(
                "magnet:?xt=urn:btih:{id}&dn=Ubuntu%2024.04%20Desktop%20amd64%20ISO\
                 &tr=udp%3A%2F%2Fa.example%3A1337%2Fannounce\
                 &tr=udp%3A%2F%2Fb.example%3A1337%2Fannounce\
                 &tr=udp%3A%2F%2Fc.example%3A1337%2Fannounce"
            )),
            torrent_url: None,
            info_url: Some("https://thepiratebay.org/description.php?id=1".into()),
        }]
    );
    assert_eq!(result.total, 1);
}

#[test]
fn a_cut_title_loses_to_the_whole_one() {
    let cut = row(&h(0xab), "Linux Command Line (Hands-On) Ubuntu Linux & Linux Scripting ...");
    let whole = row(&h(0xab), "Linux Command Line (Hands-On) Ubuntu Linux & Linux Scripting");
    let result = search(&SearchQuery::new("ubuntu"), &[fresh(&knaben(), vec![cut, whole])]);
    assert_eq!(names(&result), ["Linux Command Line (Hands-On) Ubuntu Linux & Linux Scripting"]);
}

#[test]
fn a_v2_listing_joins_the_hybrid_listing_of_the_same_torrent() {
    let (v1, v2) = (h(0x11), "22".repeat(32));
    let hybrid = Observation::new(
        Identity::Hash { hash: InfoHash::Hybrid { v1: [0x11; 20], v2: [0x22; 32] }, torrent_url: None },
        "ImageNet 2012",
    )
    .unwrap();
    let v2_only = row(&v2, "ImageNet 2012 train");
    let academic = source("academictorrents", 4, true, true);

    let result =
        search(&SearchQuery::new("imagenet"), &[fresh(&academic, vec![hybrid]), fresh(&knaben(), vec![v2_only])]);

    assert_eq!(result.releases.len(), 1);
    let release = &result.releases[0];
    assert_eq!(release.id.to_ref(), v1);
    assert_eq!(release.sources, ["academictorrents", "knaben"]);
    assert_eq!(
        release.magnet.as_deref(),
        Some(format!("magnet:?xt=urn:btih:{v1}&xt=urn:btmh:1220{v2}&dn=ImageNet%202012%20train").as_str())
    );
}

#[test]
fn conflicting_hybrids_resolve_the_same_way_in_any_order() {
    let hybrid = |v1: u8| {
        let hash = InfoHash::Hybrid { v1: [v1; 20], v2: [0x22; 32] };
        Observation::new(Identity::Hash { hash, torrent_url: None }, "ImageNet 2012").unwrap()
    };
    let q = SearchQuery::new("imagenet");
    let ids = |rows: Vec<Observation>| -> Vec<(String, String)> {
        search(&q, &[fresh(&knaben(), rows)]).releases.into_iter().map(|r| (r.id.to_ref(), r.name)).collect()
    };
    let v2_only = || row(&"22".repeat(32), "ImageNet 2012 train");
    let one_way = ids(vec![hybrid(0x02), hybrid(0x01), v2_only()]);
    let other_way = ids(vec![v2_only(), hybrid(0x01), hybrid(0x02)]);
    assert_eq!(one_way, [(h(0x02), "ImageNet 2012".to_string()), (h(0x01), "ImageNet 2012 train".to_string())]);
    assert_eq!(other_way, one_way);
}

#[test]
fn private_links_stay_out_of_results_and_with_their_own_listing() {
    let torznab = source("tracker", 8, false, false);
    let mut public = row(&h(0x33), "Debian 13 netinst");
    public.trackers = vec!["udp://public.example:6969/announce".into()];
    public.info_url = Some("https://knaben.org/t/33".into());
    let mut private = row(&h(0x33), "Debian 13 netinst");
    private.trackers = vec!["https://private.example/announce?passkey=secret".into()];
    private.info_url = Some("https://private.example/details.php?id=33&passkey=secret".into());
    let mut private_only = Observation::new(
        Identity::Hash {
            hash: infohash(&h(0x44)).unwrap(),
            torrent_url: Some("http://127.0.0.1:9117/dl/x/?jackett_apikey=k3y".into()),
        },
        "Debian 13 DVD",
    )
    .unwrap();
    private_only.trackers = vec!["https://private.example/announce?passkey=secret".into()];
    private_only.info_url = Some("https://private.example/details.php?id=44&passkey=secret".into());
    let q = SearchQuery::new("debian 13");

    let (shown, linked) = assemble(
        &q,
        Profile::default(),
        &Terms::parse(&q.text),
        &[fresh(&knaben(), vec![public]), fresh(&torznab, vec![private, private_only])],
        0,
    )
    .unwrap();

    let links = |releases: &[Release]| -> Vec<[Option<String>; 3]> {
        releases.iter().map(|r| [r.magnet.clone(), r.torrent_url.clone(), r.info_url.clone()]).collect()
    };
    let public_magnet = format!(
        "magnet:?xt=urn:btih:{}&dn=Debian%2013%20netinst&tr=udp%3A%2F%2Fpublic.example%3A6969%2Fannounce",
        h(0x33)
    );
    let public_info = Some("https://knaben.org/t/33".to_string());
    assert_eq!(
        links(&shown.releases),
        [
            [Some(format!("magnet:?xt=urn:btih:{}&dn=Debian%2013%20DVD", h(0x44))), None, None],
            [Some(public_magnet.clone()), None, public_info.clone()],
        ]
    );
    let linked: Vec<Release> = linked.into_iter().map(|(release, _)| release).collect();
    assert_eq!(
        links(&linked),
        [
            [
                Some(format!(
                    "magnet:?xt=urn:btih:{}&dn=Debian%2013%20DVD&tr=https%3A%2F%2Fprivate.example%2Fannounce%3Fpasskey%3Dsecret",
                    h(0x44)
                )),
                Some("http://127.0.0.1:9117/dl/x/?jackett_apikey=k3y".into()),
                Some("https://private.example/details.php?id=44&passkey=secret".into()),
            ],
            [Some(public_magnet), None, public_info],
        ]
    );
}

#[test]
fn locator_rows_get_a_stable_u_ref() {
    let url = "http://127.0.0.1:9117/dl/privatehd/?path=def";
    let locator = || Observation::new(Identity::Locator { torrent_url: url.into() }, "Ubuntu 26.04 LTS").unwrap();
    let torznab = source("tracker", 8, false, false);
    let q = SearchQuery::new("ubuntu");
    let (first, linked) =
        assemble(&q, Profile::default(), &Terms::parse(&q.text), &[fresh(&torznab, vec![locator()])], 0).unwrap();
    let again = search(&q, &[fresh(&torznab, vec![locator(), locator()])]);
    let digest: [u8; 20] = Sha1::digest(url.as_bytes()).into();
    assert_eq!(first.releases[0].id, ReleaseId::Locator(LocatorId::from_bytes(digest)));
    assert!(first.releases[0].id.to_ref().starts_with('u'));
    assert_eq!((first.releases[0].torrent_url.as_deref(), first.releases[0].magnet.as_deref()), (None, None));
    assert_eq!((linked[0].0.torrent_url.as_deref(), linked[0].1.as_deref()), (Some(url), Some("tracker")));
    assert_eq!(again.releases, first.releases);
}

#[test]
fn the_guard_drops_a_source_that_answered_another_question() {
    let terms = Terms::parse("ubuntu");
    let latest = vec![
        row(&h(1), "Some Movie 2026 1080p"),
        row(&h(2), "Another Show S01E01"),
        row(&h(3), "Ubuntu 24.04"),
        row(&h(4), "Album 2026 FLAC"),
    ];
    assert_eq!(vet(&terms, latest), Err(Failure::Irrelevant));

    let search_rows = vec![row(&h(1), "Ubuntu 24.04"), row(&h(2), "ubuntu-26.04-desktop"), row(&h(3), "Unrelated")];
    let kept: Vec<String> = vet(&terms, search_rows).unwrap().iter().map(|o| o.name().to_string()).collect();
    assert_eq!(kept, ["Ubuntu 24.04", "ubuntu-26.04-desktop"]);
}

fn films() -> Vec<Run> {
    let mut lawful = with(row(&h(0xa1), "Night of the Living Dead (1968)"), None, Some(5_000), Some(100));
    lawful.category = Some(Category::Video);
    let bluray = with(row(&h(0xb2), "Night of the Living Dead 1968 1080p BluRay"), Some(900), Some(4_000), Some(500));
    let remake = with(row(&h(0xc3), "Night of the Living Dead (1990)"), Some(5_000), Some(1_000), Some(300));
    let partial = with(row(&h(0xd4), "Living Dead Night Collection"), Some(10_000), Some(3_000), None);
    let mirror = with(row(&h(0xe5), "Night of the Living Dead 1968"), Some(900), Some(2_000), Some(500));
    vec![
        fresh(&archive(), vec![lawful]),
        fresh(&knaben(), vec![bluray, remake, partial]),
        fresh(&apibay(), vec![mirror]),
    ]
}

#[test]
fn ranks_relevance_before_popularity_then_lawful_seeders_extra_words() {
    let result = search(&SearchQuery::new("night of the living dead 1968"), &films());
    assert_eq!(
        names(&result),
        [
            "Night of the Living Dead (1968)",
            "Night of the Living Dead 1968",
            "Night of the Living Dead 1968 1080p BluRay",
            "Night of the Living Dead (1990)",
            "Living Dead Night Collection",
        ]
    );
}

const GB: u64 = 1_000_000_000;

fn remasters() -> Vec<Run> {
    let film = |byte, tags: &str, seeders, size| {
        let mut o =
            with(row(&h(byte), &format!("Night of the Living Dead 1968 {tags}")), Some(seeders), Some(size), None);
        o.category = Some(Category::Video);
        o
    };
    let mut lawful = with(row(&h(0x10), "Night of the Living Dead 1968 4K Remaster"), None, Some(58 * GB), None);
    lawful.category = Some(Category::Video);
    vec![
        fresh(&archive(), vec![lawful]),
        fresh(
            &knaben(),
            vec![
                film(0x11, "2160p UHD BluRay HDR10 x265", 500, 20 * GB),
                film(0x12, "2160p WEB x265", 300, 9 * GB),
                film(0x13, "1080p BluRay x264", 200, 8 * GB),
                film(0x14, "1080p WEB x265", 150, 2 * GB),
                film(0x15, "720p BluRay x264", 100, 1_500_000_000),
                film(0x16, "DVDRip", 80, 700_000_000),
                film(0x17, "Restored", 600, GB),
            ],
        ),
    ]
}

fn machine(locale: &str, display: Option<Display>, arch: Arch) -> Machine {
    Machine { os: Os::Macos, display, arch: Some(arch), language: Language::from_locale(locale) }
}

fn mac() -> Profile {
    let panel = Display { width: 3024, height: 1964, hdr: true };
    Profile { detected: Some(machine("en-IN", Some(panel), Arch::Arm64)), ..Profile::default() }
}

fn preferring(preferences: Preferences) -> Profile {
    Profile { preferences, ..Profile::default() }
}

fn ranked(profile: Profile) -> Vec<String> {
    let q = SearchQuery { profile, ..SearchQuery::new("night of the living dead 1968") };
    names(&search(&q, &remasters()))
        .iter()
        .map(|n| n.trim_start_matches("Night of the Living Dead 1968 ").to_string())
        .collect()
}

#[test]
fn without_a_profile_lawful_sources_then_seeders_lead() {
    assert_eq!(
        ranked(Profile::default()),
        [
            "4K Remaster",
            "Restored",
            "2160p UHD BluRay HDR10 x265",
            "2160p WEB x265",
            "1080p BluRay x264",
            "1080p WEB x265",
            "720p BluRay x264",
            "DVDRip",
        ]
    );
}

#[test]
fn a_screen_ranks_its_resolution_first_and_unknown_resolutions_last() {
    assert_eq!(
        ranked(mac()),
        [
            "4K Remaster",
            "1080p BluRay x264",
            "1080p WEB x265",
            "720p BluRay x264",
            "2160p UHD BluRay HDR10 x265",
            "2160p WEB x265",
            "DVDRip",
            "Restored",
        ]
    );
    let on = |screen| {
        ranked(Profile { preferences: Preferences { screen: Some(screen), ..Preferences::default() }, ..mac() })
    };
    assert_eq!(
        on(Screen::Tv4k),
        [
            "4K Remaster",
            "2160p UHD BluRay HDR10 x265",
            "2160p WEB x265",
            "1080p BluRay x264",
            "1080p WEB x265",
            "720p BluRay x264",
            "DVDRip",
            "Restored",
        ]
    );
    assert_eq!(
        on(Screen::Phone),
        [
            "4K Remaster",
            "720p BluRay x264",
            "DVDRip",
            "1080p BluRay x264",
            "1080p WEB x265",
            "2160p UHD BluRay HDR10 x265",
            "2160p WEB x265",
            "Restored",
        ]
    );
}

#[test]
fn an_sdr_display_ranks_hdr_after_everything_it_shows_well() {
    let monitor = Display { width: 1920, height: 1080, hdr: false };
    let linux = Profile { detected: Some(machine("en_US.UTF-8", Some(monitor), Arch::X86_64)), ..Profile::default() };
    assert_eq!(
        ranked(linux),
        [
            "4K Remaster",
            "1080p BluRay x264",
            "1080p WEB x265",
            "720p BluRay x264",
            "2160p WEB x265",
            "DVDRip",
            "Restored",
            "2160p UHD BluRay HDR10 x265",
        ]
    );
}

#[test]
fn a_size_cap_or_a_full_disk_ranks_what_fits_first_even_before_lawful_sources() {
    let expected = [
        "Restored",
        "1080p BluRay x264",
        "1080p WEB x265",
        "720p BluRay x264",
        "DVDRip",
        "4K Remaster",
        "2160p UHD BluRay HDR10 x265",
        "2160p WEB x265",
    ];
    assert_eq!(ranked(preferring(Preferences { max_size: Some(8 * GB), ..Preferences::default() })), expected);
    assert_eq!(ranked(Profile { room: Some(8 * GB), ..Profile::default() }), expected);
}

#[test]
fn a_player_ranks_what_it_may_not_play_last() {
    let for_player = |playback| ranked(preferring(Preferences { playback: Some(playback), ..Preferences::default() }));
    assert_eq!(for_player(Playback::HevcHdr), ranked(Profile::default()));
    assert_eq!(
        for_player(Playback::Hevc),
        [
            "4K Remaster",
            "Restored",
            "2160p WEB x265",
            "1080p BluRay x264",
            "1080p WEB x265",
            "720p BluRay x264",
            "DVDRip",
            "2160p UHD BluRay HDR10 x265",
        ]
    );
    assert_eq!(
        for_player(Playback::H264),
        [
            "4K Remaster",
            "Restored",
            "1080p BluRay x264",
            "720p BluRay x264",
            "DVDRip",
            "2160p UHD BluRay HDR10 x265",
            "2160p WEB x265",
            "1080p WEB x265",
        ]
    );
}

#[test]
fn every_preference_together_ranks_fit_then_lawful_then_screen() {
    let laptop = Preferences { screen: Some(Screen::Laptop), max_size: Some(2 * GB), playback: Some(Playback::H264) };
    assert_eq!(
        ranked(Profile { preferences: laptop, ..mac() }),
        [
            "720p BluRay x264",
            "DVDRip",
            "Restored",
            "4K Remaster",
            "1080p BluRay x264",
            "1080p WEB x265",
            "2160p UHD BluRay HDR10 x265",
            "2160p WEB x265",
        ]
    );
}

#[test]
fn a_tagged_trailer_ranks_after_the_film_on_a_screen() {
    let film = with(row(&h(0x20), "Night of the Living Dead 1968"), Some(40), Some(GB), None);
    let trailer = with(row(&h(0x21), "Night of the Living Dead 1968 Trailer HD 1080p"), Some(40), Some(GB / 10), None);
    let runs = [fresh(&knaben(), vec![trailer, film])];
    let ranked = |profile| {
        let q = SearchQuery { profile, ..SearchQuery::new("night of the living dead 1968") };
        names(&search(&q, &runs)).iter().map(|n| n.to_string()).collect::<Vec<_>>()
    };
    let film_first = ["Night of the Living Dead 1968", "Night of the Living Dead 1968 Trailer HD 1080p"];
    assert_eq!(ranked(Profile::default()), film_first);
    assert_eq!(ranked(mac()), film_first);
}

#[test]
fn a_screen_leaves_releases_that_are_not_video_to_seeders() {
    let mut episode = with(
        row(&h(0x50), "Celtics City S01E08 Chapter VIII Ubuntu 1080p AMZN WEB-DL DDP5 1 H 264-Kits"),
        Some(1),
        Some(4 * GB),
        None,
    );
    episode.category = Some(Category::Video);
    let iso = with(row(&h(0x51), "ubuntu-26.04-desktop-amd64.iso"), Some(143), Some(6 * GB), None);
    let runs = [fresh(&knaben(), vec![episode, iso])];
    let q = SearchQuery { profile: mac(), ..SearchQuery::new("ubuntu") };
    assert_eq!(
        names(&search(&q, &runs)),
        [
            "ubuntu-26.04-desktop-amd64.iso",
            "Celtics City S01E08 Chapter VIII Ubuntu 1080p AMZN WEB-DL DDP5 1 H 264-Kits"
        ]
    );
}

#[test]
fn the_machines_architecture_breaks_a_tie_before_seeders() {
    let runs = [fresh(
        &knaben(),
        vec![
            with(row(&h(0x30), "debian-13.7.0-amd64-netinst.iso"), Some(50), Some(GB), None),
            with(row(&h(0x31), "debian-13.7.0-arm64-netinst.iso"), Some(10), Some(GB), None),
            with(row(&h(0x32), "debian-13.7.0-source-netinst.iso"), Some(30), Some(GB), None),
        ],
    )];
    let ranked = |profile| {
        let q = SearchQuery { profile, ..SearchQuery::new("debian 13.7 netinst") };
        names(&search(&q, &runs)).iter().map(|n| n.to_string()).collect::<Vec<_>>()
    };
    let pc = Profile { detected: Some(machine("en", None, Arch::X86_64)), ..Profile::default() };
    assert_eq!(
        ranked(mac()),
        ["debian-13.7.0-arm64-netinst.iso", "debian-13.7.0-amd64-netinst.iso", "debian-13.7.0-source-netinst.iso"]
    );
    assert_eq!(
        ranked(pc),
        ["debian-13.7.0-amd64-netinst.iso", "debian-13.7.0-source-netinst.iso", "debian-13.7.0-arm64-netinst.iso"]
    );
    assert_eq!(
        ranked(Profile::default()),
        ["debian-13.7.0-amd64-netinst.iso", "debian-13.7.0-source-netinst.iso", "debian-13.7.0-arm64-netinst.iso"]
    );
}

#[test]
fn the_system_language_breaks_a_tie_before_seeders() {
    let runs = [fresh(
        &archive(),
        vec![
            with(row(&h(0x40), "Night Of The Living Dead ( 1968) Doblada Al Español FULL HD"), Some(9), None, None),
            with(row(&h(0x41), "Night Of The Living Dead ( 1968) English FULL HD"), Some(3), None, None),
        ],
    )];
    let ranked = |profile| {
        let q = SearchQuery { profile, ..SearchQuery::new("night of the living dead 1968") };
        names(&search(&q, &runs)).iter().map(|n| n.to_string()).collect::<Vec<_>>()
    };
    let english_first = [
        "Night Of The Living Dead ( 1968) English FULL HD",
        "Night Of The Living Dead ( 1968) Doblada Al Español FULL HD",
    ];
    let spanish_first = [english_first[1], english_first[0]];
    let mexico = Profile { detected: Some(machine("es-MX", None, Arch::Arm64)), ..Profile::default() };
    assert_eq!(ranked(mac()), english_first);
    assert_eq!(ranked(mexico), spanish_first);
    assert_eq!(ranked(Profile::default()), spanish_first);
}

#[test]
fn the_result_carries_the_profile_it_ranked_for_in_place_of_the_querys() {
    let q = SearchQuery::new("night of the living dead 1968");
    let (result, _) = assemble(&q, mac(), &Terms::parse(&q.text), &remasters(), 0).unwrap();
    assert_eq!((q.profile, result.query.profile), (Profile::default(), mac()));
}

#[test]
fn a_file_too_small_for_its_tag_ranks_as_untagged_on_a_screen() {
    let laptop = Preferences { screen: Some(Screen::Laptop), ..Preferences::default() };
    let ranked = |text: &str, rows: Vec<Observation>| {
        let q = SearchQuery { profile: preferring(laptop), ..SearchQuery::new(text) };
        names(&search(&q, &[fresh(&knaben(), rows)])).iter().map(|n| n.to_string()).collect::<Vec<_>>()
    };
    let sized = |byte, name, size| with(row(&h(byte), name), Some(40), Some(size), None);
    let clip = sized(0x22, "Night of the Living Dead |1968| WebRip US (HD 1080p)", 8_690_000);
    let film = sized(0x23, "Night of the Living Dead (1968)", GB);
    assert_eq!(
        ranked("night of the living dead 1968", vec![clip, film]),
        ["Night of the Living Dead (1968)", "Night of the Living Dead |1968| WebRip US (HD 1080p)"]
    );
    let episodes = vec![
        sized(0x24, "Bobs Burgers S16E03", 300_000_000),
        sized(0x25, "Bobs Burgers S16E03 1080p HEVC x265-MeGusta", 112_500_000),
        sized(0x26, "Bobs Burgers S16E03 720p WEB x265-MiNX", 81_400_000),
    ];
    assert_eq!(
        ranked("bobs burgers s16e03", episodes),
        [
            "Bobs Burgers S16E03 1080p HEVC x265-MeGusta",
            "Bobs Burgers S16E03 720p WEB x265-MiNX",
            "Bobs Burgers S16E03"
        ]
    );
}

#[test]
fn alternate_sorts_keep_the_same_tie_breaks() {
    let sorted = |sort| {
        let mut q = SearchQuery::new("night of the living dead 1968");
        q.sort = sort;
        names(&search(&q, &films())).into_iter().map(str::to_string).collect::<Vec<_>>()
    };
    assert_eq!(
        sorted(Sort::Seeds),
        [
            "Living Dead Night Collection",
            "Night of the Living Dead (1990)",
            "Night of the Living Dead 1968",
            "Night of the Living Dead 1968 1080p BluRay",
            "Night of the Living Dead (1968)",
        ]
    );
    assert_eq!(
        sorted(Sort::Size),
        [
            "Night of the Living Dead (1968)",
            "Night of the Living Dead 1968 1080p BluRay",
            "Living Dead Night Collection",
            "Night of the Living Dead 1968",
            "Night of the Living Dead (1990)",
        ]
    );
    assert_eq!(
        sorted(Sort::Age),
        [
            "Night of the Living Dead 1968",
            "Night of the Living Dead 1968 1080p BluRay",
            "Night of the Living Dead (1990)",
            "Night of the Living Dead (1968)",
            "Living Dead Night Collection",
        ]
    );
}

#[test]
fn filters_exclude_unknown_values_and_count_before_the_limit() {
    let mut q = SearchQuery::new("night of the living dead 1968");
    let mut runs = films();
    runs.push(fresh(
        &source("nyaa", 7, false, true),
        vec![with(row(&h(0xf6), "Night of the Living Dead 1968 REMUX"), Some(40), None, None)],
    ));

    q.max_size = Some(3_000);
    let small = search(&q, &runs);
    assert_eq!(
        names(&small),
        ["Night of the Living Dead 1968", "Night of the Living Dead (1990)", "Living Dead Night Collection"]
    );

    q.max_size = None;
    q.min_seeds = Some(1_000);
    q.limit = 1;
    let seeded = search(&q, &runs);
    assert_eq!(names(&seeded), ["Night of the Living Dead (1990)"]);
    assert_eq!(seeded.total, 2);

    q.min_seeds = None;
    q.limit = 10;
    q.category = Some(Category::Video);
    assert_eq!(names(&search(&q, &runs)), ["Night of the Living Dead (1968)"]);
}

#[test]
fn hides_adult_releases_without_eating_lookalike_titles() {
    let mut film = row(&h(0x01), "xXx (2002) 1080p BluRay");
    film.category = Some(Category::Video);
    let by_category = row(&h(0x02), "xXx 2002 scene pack").flag(Flag::Adult, true);
    let by_name = row(&h(0x03), "Brazzers xXx 2002");
    let show = row(&h(0x04), "Sex Education S04 1080p");
    let runs = [fresh(&knaben(), vec![film, by_category, by_name, show])];

    let hidden = search(&SearchQuery::new("xxx 2002 sex education"), &runs);
    assert_eq!(names(&hidden), ["xXx (2002) 1080p BluRay", "Sex Education S04 1080p"]);
    assert_eq!((hidden.total, hidden.hidden_adult), (2, 2));

    let mut q = SearchQuery::new("xxx 2002 sex education");
    q.adult = true;
    let shown = search(&q, &runs);
    let adult: Vec<&str> =
        shown.releases.iter().filter(|r| r.flags.contains(&Flag::Adult)).map(|r| r.name.as_str()).collect();
    assert_eq!(adult, ["Brazzers xXx 2002", "xXx 2002 scene pack"]);
    assert_eq!((shown.total, shown.hidden_adult), (4, 0));
}

#[test]
fn flags_camera_releases_from_any_listing_name() {
    let mut cam = row(&h(0x05), "Some Movie 2024 HDCAM x264");
    cam.category = Some(Category::Video);
    let result = search(&SearchQuery::new("some movie 2024"), &[fresh(&knaben(), vec![cam])]);
    assert_eq!(result.releases[0].flags, BTreeSet::from([Flag::Cam]));
}

#[test]
fn every_source_failing_is_an_error_not_an_empty_list() {
    let q = SearchQuery::new("ubuntu");
    let runs = [
        failed(&apibay(), Failure::Http(503)),
        failed(&knaben(), Failure::Timeout),
        failed(&archive(), Failure::Irrelevant),
    ];
    let err = assemble(&q, Profile::default(), &Terms::parse("ubuntu"), &runs, 6000).unwrap_err();
    assert_eq!(err.code, ErrorCode::ProvidersUnavailable);
    assert_eq!(err.message, "all sources failed (archive.org, knaben, apibay)");
    assert_eq!(
        err.details,
        Some(Details::Sources(vec![
            SourceReport { source: "archive.org".into(), outcome: SourceOutcome::Irrelevant, elapsed_ms: 6000 },
            SourceReport { source: "knaben".into(), outcome: SourceOutcome::Timeout, elapsed_ms: 6000 },
            SourceReport { source: "apibay".into(), outcome: SourceOutcome::Http { status: 503 }, elapsed_ms: 6000 },
        ]))
    );
}

#[test]
fn one_empty_answer_is_a_successful_search_with_no_results() {
    let runs = [failed(&apibay(), Failure::Http(503)), fresh(&knaben(), Vec::new())];
    let result = search(&SearchQuery::new("zzqqxx"), &runs);
    assert_eq!(result.releases, []);
    assert_eq!(
        result.sources,
        [
            SourceReport { source: "knaben".into(), outcome: SourceOutcome::Ok { results: 0 }, elapsed_ms: 40 },
            SourceReport { source: "apibay".into(), outcome: SourceOutcome::Http { status: 503 }, elapsed_ms: 6000 },
        ]
    );
}

struct XorShift(u64);

impl XorShift {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

fn crowded() -> Vec<(Arc<SourceMeta>, Vec<Observation>)> {
    let titles = [
        "Night of the Living Dead 1968",
        "Night of the Living Dead (1968) 1080p",
        "Night of the Living Dead 1990",
        "Living Dead",
        "Night of the Living Dead",
    ];
    let sources = [archive(), knaben(), apibay(), source("tracker", 8, false, false)];
    let mut by_source: Vec<(Arc<SourceMeta>, Vec<Observation>)> =
        sources.iter().map(|s| (s.clone(), Vec::new())).collect();
    for i in 0..60u32 {
        let id = u8::try_from(i % 23 + 1).unwrap();
        let mut o = row(&h(id), titles[i as usize % titles.len()]);
        o.peers = [None, Some(Peers { seeders: 10, leechers: Some(1) }), Some(Peers { seeders: 10, leechers: None })]
            [i as usize % 3];
        o.size = [None, Some(700), Some(700), Some(1400)][i as usize % 4];
        o.published = [None, Some(50), Some(50)][i as usize % 3];
        o.trackers = vec![format!("udp://t{}.example:80/announce", i % 5)];
        if i % 11 == 0 {
            o.identity = Identity::Locator { torrent_url: format!("https://example.org/{}.torrent", i % 2) };
        }
        by_source[i as usize % sources.len()].1.push(o);
    }
    let hybrid = Observation::new(
        Identity::Hash { hash: InfoHash::Hybrid { v1: [0x01; 20], v2: [0xfe; 32] }, torrent_url: None },
        "Night of the Living Dead 1968 hybrid",
    )
    .unwrap();
    by_source[0].1.push(hybrid);
    by_source[1].1.push(row(&"fe".repeat(32), "Night of the Living Dead 1968"));
    by_source
}

#[test]
fn a_thousand_shuffles_give_one_order() {
    let query = SearchQuery { limit: 100, ..SearchQuery::new("night of the living dead 1968") };
    let terms = Terms::parse(&query.text);
    let base = crowded();
    let runs_from = |groups: &[(Arc<SourceMeta>, Vec<Observation>)]| -> Vec<Run> {
        groups.iter().map(|(s, rows)| fresh(s, rows.clone())).collect()
    };
    let (expected, expected_linked) = assemble(&query, Profile::default(), &terms, &runs_from(&base), 0).unwrap();
    assert_eq!(expected.releases.len(), 25);
    let hybrid = expected.releases.iter().find(|r| r.id.to_ref() == h(0x01)).unwrap();
    assert_eq!(hybrid.name, "Night of the Living Dead (1968) 1080p");
    assert_eq!(hybrid.sources, ["archive.org", "knaben", "apibay", "tracker"]);
    assert!(hybrid.magnet.as_deref().unwrap().contains(&format!("xt=urn:btmh:1220{}", "fe".repeat(32))));

    let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
    for _ in 0..1000 {
        let mut groups = base.clone();
        rng.shuffle(&mut groups);
        for (_, rows) in &mut groups {
            rng.shuffle(rows);
        }
        let (shuffled, linked) = assemble(&query, Profile::default(), &terms, &runs_from(&groups), 0).unwrap();
        assert_eq!(shuffled.releases, expected.releases);
        assert_eq!(linked, expected_linked);
        assert_eq!(shuffled.sources, expected.sources);
    }
}
