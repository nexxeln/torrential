//! How releases order: what fits the player and the disk, how close a resolution is to the
//! screen, how a swarm stands, and the chain that sorts a search's candidates by them.

use std::cmp::{Ordering, Reverse};
use std::ops::RangeInclusive;

use torrential_core::title::{Span, Wanted};
use torrential_core::{Arch, Category, Flag, Language, Profile, Quality, Resolution, Sort, Swarm};

use crate::assemble::Candidate;
use crate::terms;

/// Below this many Mbit/s a release is a fake or a broken file.
pub(crate) const FAKE_MBPS: f64 = 0.25;

/// The tier of a swarm with `seeders`, listed by a lawful catalog or not. `None` for a swarm
/// known to have no seeder outside a lawful catalog. A lawful catalog that does not count
/// seeders serves its files from web seeds over HTTP, as dependable as any swarm.
pub fn swarm(seeders: Option<u32>, lawful: bool) -> Option<Swarm> {
    match (seeders, lawful) {
        (Some(20..), _) | (None, true) => Some(Swarm::Strong),
        (Some(5..=19), _) => Some(Swarm::Fair),
        (Some(1..=4), _) => Some(Swarm::Weak),
        (Some(0), true) | (None, false) => Some(Swarm::Unknown),
        (Some(0), false) => None,
    }
}

/// The Mbit/s at each resolution that looks right and streams comfortably. A small encode of a
/// popular release sits low in it, and a remux runs above it.
fn band(resolution: Option<Resolution>) -> RangeInclusive<f64> {
    match resolution {
        Some(Resolution::Sd) => 0.3..=4.0,
        Some(Resolution::P720) => 0.6..=8.0,
        Some(Resolution::P1080) => 1.0..=16.0,
        Some(Resolution::P2160) => 3.0..=40.0,
        None => 0.3..=16.0,
    }
}

/// Whether a release is too small to be what its resolution tag claims.
pub(crate) fn is_too_small(size: Option<u64>, resolution: Resolution) -> bool {
    let floor = match resolution {
        Resolution::Sd => 0,
        Resolution::P720 | Resolution::P1080 => 50_000_000,
        Resolution::P2160 => 500_000_000,
    };
    size.is_some_and(|size| size < floor)
}

/// False only for a release known to be larger than `max_size` or the room on disk, known to
/// need a codec or HDR the player may not handle, or known to stream to a browser at more `mbps`
/// than its screen shows. Unknown values fit.
fn fits(profile: &Profile, size: Option<u64>, quality: Quality, mbps: Option<f64>) -> bool {
    let within = |cap: Option<u64>| size.zip(cap).is_none_or(|(size, cap)| size <= cap);
    within(profile.preferences.max_size)
        && within(profile.room)
        && profile.playback().is_none_or(|playback| playback.value.plays(quality))
        && mbps.zip(most_mbps(profile)).is_none_or(|(mbps, most)| mbps <= most)
}

/// The most Mbit/s a play may stream to a browser: the top of the band of the resolution its
/// screen wants, where remuxes start. So a phone takes up to 8, about 6 GB for a film of 100
/// minutes, a 1080p screen up to 16, and a 4K TV up to 40. More shows the screen nothing new and
/// only makes the download take longer. The player this machine opens keeps the rules it had.
fn most_mbps(profile: &Profile) -> Option<f64> {
    if !profile.player.streams() {
        return None;
    }
    Some(*band(Some(profile.screen()?.value)).end())
}

/// Lower is closer to the screen's resolution: the same, then one step lower, then one higher,
/// then two lower, and so on. A bigger picture than the screen shows wastes bytes, a smaller one
/// loses detail. Without a screen every release ties at 0. With one, an unknown resolution comes
/// after every known one, since a name without a tag gives no sign that it suits the screen.
fn closeness(profile: &Profile, resolution: Option<Resolution>) -> u8 {
    let step = |r: Resolution| match r {
        Resolution::Sd => 0i8,
        Resolution::P720 => 1,
        Resolution::P1080 => 2,
        Resolution::P2160 => 3,
    };
    match (profile.screen(), resolution) {
        (None, _) => 0,
        (Some(screen), Some(resolution)) => {
            let steps = step(resolution) - step(screen.value);
            if steps < 0 { steps.unsigned_abs() * 2 - 1 } else { steps.unsigned_abs() * 2 }
        }
        (Some(_), None) => u8::MAX,
    }
}

/// 0 for a name that mentions this machine's architecture, and 1 for any other. A name for
/// another architecture ranks no lower than one without, so on an arm64 Mac a search for Ubuntu
/// still leads with its amd64 ISOs, not with TV episodes that mention Ubuntu.
fn arch_rank(profile: &Profile, named: &[Arch]) -> u8 {
    u8::from(profile.arch().is_some_and(|host| !named.contains(&host)))
}

/// 0 for a name that marks the system language, 1 for one in the original language, and 2 for
/// one that marks only others. The original is usually English, so a name in English or without
/// a marker is the original, and for English it ranks first.
fn language_rank(profile: &Profile, named: &[Language]) -> u8 {
    let Some(own) = profile.language() else { return 0 };
    let original = named.is_empty() || named.iter().any(|l| l.code() == "en");
    if named.contains(own) || (original && own.code() == "en") {
        0
    } else if original {
        1
    } else {
        2
    }
}

/// Mbit/s per film or episode, from the size and the catalog's runtime. `None` when either is
/// unknown, and for a release of every season, whose episodes are not counted.
pub(crate) fn bitrate(size: Option<u64>, span: Span, wanted: &Wanted) -> Option<f64> {
    let episodes = match span {
        Span::Film | Span::Episode(_) => 1,
        Span::Season(_) => wanted.season_episodes?,
        Span::Seasons { first, last } => wanted.season_episodes? * (last - first + 1),
        Span::Complete => return None,
    };
    let seconds = wanted.runtime?.as_secs_f64() * f64::from(episodes);
    (seconds > 0.0).then(|| size.map(|size| size as f64 * 8.0 / seconds / 1e6))?
}

/// Play preference for a release of the wanted title. Derived `Ord`, so lower plays first.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct PlayRank {
    cam: bool,
    unfit: bool,
    /// The name marks languages, and none is the system's or the original English.
    foreign: bool,
    swarm: Swarm,
    undated: bool,
    standing: Reverse<u8>,
    /// Ahead of the screen's resolution: a remux that fits the screen still does not stream.
    off_band: bool,
    other_words: usize,
    span: u8,
    closeness: u8,
    /// Most first. `Reverse` puts unknown after every count.
    seeders: Reverse<Option<u32>>,
}

/// How much more than the film or episode a release holds: none, a season, several, or all.
fn breadth(span: Option<Span>) -> u8 {
    match span {
        None | Some(Span::Film | Span::Episode(_)) => 0,
        Some(Span::Season(_)) => 1,
        Some(Span::Seasons { .. }) => 2,
        Some(Span::Complete) => 3,
    }
}

/// In a title's search the play order leads: not a cam, fits, not in another language, swarm,
/// names the year, standing, bitrate, a plain name, span, screen, and seeders. The chain after
/// it orders every search.
pub(crate) fn rank(candidates: &mut [Candidate], sort: Sort, profile: &Profile) {
    let fits = |c: &Candidate| fits(profile, c.release.size, c.quality, c.title.as_ref().and_then(|t| t.mbps));
    let is_video = |c: &Candidate| c.release.category == Some(Category::Video) || c.quality.resolution.is_some();
    let video_search = candidates.iter().filter(|c| is_video(c)).count() * 2 > candidates.len();
    let closeness_here = |c: &Candidate| if video_search { closeness(profile, c.resolution) } else { 0 };
    let play = |c: &Candidate| {
        c.title.as_ref().map(|t| PlayRank {
            cam: c.release.flags.contains(&Flag::Cam),
            unfit: !fits(c),
            foreign: language_rank(profile, &c.languages) == 2,
            swarm: t.swarm,
            undated: t.undated,
            standing: Reverse(standing(c)),
            off_band: t.mbps.is_some_and(|mbps| !band(c.quality.resolution).contains(&mbps)),
            other_words: t.other_words,
            span: breadth(c.release.span),
            closeness: closeness(profile, c.resolution),
            seeders: Reverse(c.release.seeders),
        })
    };
    candidates.sort_by(|a, b| {
        let (ra, rb) = (&a.release, &b.release);
        let primary = play(a).cmp(&play(b)).then(match sort {
            Sort::Relevance => Ordering::Equal,
            Sort::Seeds => known_first_desc(ra.seeders, rb.seeders),
            Sort::Size => known_first_desc(ra.size, rb.size),
            Sort::Age => known_first_desc(ra.published, rb.published),
        });
        primary
            .then_with(|| b.relevance.cmp(&a.relevance))
            .then_with(|| fits(b).cmp(&fits(a)))
            .then_with(|| standing(b).cmp(&standing(a)))
            .then_with(|| closeness_here(a).cmp(&closeness_here(b)))
            .then_with(|| arch_rank(profile, &a.arches).cmp(&arch_rank(profile, &b.arches)))
            .then_with(|| language_rank(profile, &a.languages).cmp(&language_rank(profile, &b.languages)))
            .then_with(|| known_first_desc(ra.seeders, rb.seeders))
            .then_with(|| a.extra_words.cmp(&b.extra_words))
            .then_with(|| known_first_desc(a.downloads, b.downloads))
            .then_with(|| terms::newer_first(&ra.name, &rb.name))
            .then_with(|| ra.id.to_ref().cmp(&rb.id.to_ref()))
    });
}

fn standing(c: &Candidate) -> u8 {
    if c.release.flags.contains(&Flag::Official) { 2 } else { u8::from(c.lawful) }
}

fn known_first_desc<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => b.cmp(&a),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrential_core::{Display, Machine, Os, Playback, Player, Preferences, Screen, Viewer};

    fn mac() -> Machine {
        Machine {
            os: Os::Macos,
            display: Some(Display { width: 3024, height: 1964, hdr: true }),
            arch: Some(Arch::Arm64),
            language: Language::from_locale("en-IN"),
        }
    }

    fn server() -> Machine {
        Machine { os: Os::Linux, display: None, arch: Some(Arch::X86_64), language: Language::from_locale("C.UTF-8") }
    }

    fn profile(detected: Option<Machine>, preferences: Preferences) -> Profile {
        Profile { detected, preferences, ..Profile::default() }
    }

    fn phone() -> Viewer {
        Viewer::measured(1170, 2532, 3.0, Playback::Hevc)
    }

    fn tv_4k() -> Viewer {
        Viewer::measured(3840, 2160, 1.0, Playback::HevcHdr)
    }

    fn playing_in(player: Player, preferences: Preferences) -> Profile {
        Profile { player, ..profile(Some(mac()), preferences) }
    }

    fn watching_on(viewer: Viewer, preferences: Preferences) -> Profile {
        playing_in(Player::Device(viewer), preferences)
    }

    #[test]
    fn a_browser_takes_no_more_mbit_than_the_band_of_its_screen() {
        let fits = |p: &Profile, mbps: [f64; 4]| mbps.map(|mbps| fits(p, Some(1), Quality::default(), Some(mbps)));
        let tv = Preferences { screen: Some(Screen::Tv4k), ..Preferences::default() };
        let rates = [8.0, 8.1, 22.0, 41.0];
        assert_eq!(fits(&watching_on(phone(), tv), rates), [true, false, false, false], "a phone");
        assert_eq!(fits(&watching_on(tv_4k(), Preferences::default()), rates), [true, true, true, false], "a 4K TV");
        let here = |preferences| playing_in(Player::Browser { playback: Playback::Hevc }, preferences);
        assert_eq!(fits(&here(Preferences::default()), rates), [true, true, false, false], "this Mac's browser");
        assert_eq!(
            fits(&here(tv), rates),
            [true, true, true, false],
            "this Mac's browser on the screen in config.yaml"
        );
        assert_eq!(fits(&playing_in(Player::External, Preferences::default()), rates), [true; 4], "this Mac's player");
        assert!(
            super::fits(&watching_on(phone(), Preferences::default()), Some(1), Quality::default(), None),
            "an unknown bitrate"
        );
    }

    #[test]
    fn an_sdr_display_ranks_hdr_as_unplayable_and_a_full_disk_ranks_big_files_out() {
        let sdr = Machine { display: Some(Display { width: 1920, height: 1080, hdr: false }), ..mac() };
        let fits = |p: &Profile, size, name| fits(p, Some(size), Quality::parse(name), None);
        let on_sdr = profile(Some(sdr), Preferences::default());
        let full = Profile { room: Some(4_000_000_000), ..profile(Some(mac()), Preferences::default()) };

        assert_eq!([fits(&on_sdr, 1, "Film 2160p HDR x265"), fits(&on_sdr, 1, "Film 2160p x265")], [false, true]);
        assert_eq!([fits(&full, 4_000_000_000, "Film"), fits(&full, 4_000_000_001, "Film")], [true, false]);
        assert!(super::fits(&full, None, Quality::default(), None), "an unknown size fits");
    }

    #[test]
    fn closeness_runs_same_then_lower_then_higher_with_unknown_last() {
        use Resolution::{P720, P1080, P2160, Sd};
        let order = |screen| {
            let p = profile(Some(mac()), Preferences { screen, ..Preferences::default() });
            [Some(P2160), Some(P1080), Some(P720), Some(Sd), None].map(|r| closeness(&p, r))
        };
        assert_eq!(order(None), [2, 0, 1, 3, u8::MAX], "this Mac's 1080p panel");
        assert_eq!(order(Some(Screen::Tv4k)), [0, 1, 3, 5, u8::MAX]);
        assert_eq!(order(Some(Screen::Phone)), [4, 2, 0, 1, u8::MAX]);
        let headless = profile(Some(server()), Preferences::default());
        assert_eq!([Some(P2160), Some(Sd), None].map(|r| closeness(&headless, r)), [0, 0, 0]);
    }

    #[test]
    fn the_host_architecture_ranks_first() {
        let arm = profile(Some(mac()), Preferences::default());
        let ranks = |p: &Profile| {
            [&[Arch::Arm64][..], &[], &[Arch::X86_64], &[Arch::X86_64, Arch::Arm64]].map(|n| arch_rank(p, n))
        };
        assert_eq!(ranks(&arm), [0, 1, 1, 0]);
        assert_eq!(ranks(&profile(None, Preferences::default())), [0, 0, 0, 0]);
    }

    #[test]
    fn the_system_language_ranks_first_and_another_last() {
        let with_locale = |locale| {
            profile(Some(Machine { language: Language::from_locale(locale), ..mac() }), Preferences::default())
        };
        let es = Language::from_locale("es").unwrap();
        let en = Language::from_locale("en").unwrap();
        let names = [vec![], vec![en.clone()], vec![es.clone()], vec![es, en]];
        let ranks = |p: Profile| names.clone().map(|n| language_rank(&p, &n));
        assert_eq!(ranks(with_locale("en-IN")), [0, 0, 2, 0]);
        assert_eq!(ranks(with_locale("es_MX.UTF-8")), [1, 1, 0, 0]);
        assert_eq!(ranks(with_locale("ja-JP")), [1, 1, 2, 1]);
    }
}
