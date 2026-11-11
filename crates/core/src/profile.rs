use serde::{Deserialize, Serialize, Serializer};

use crate::preferences::{Playback, Preferences};
use crate::quality::{Quality, Resolution};
use crate::units::{format_limit, size_text};

/// What the engine detected. Each part is `None` when detection found nothing.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Machine {
    pub os: Os,
    /// The largest display. `None` on a machine without one, such as a server.
    pub display: Option<Display>,
    pub arch: Option<Arch>,
    pub language: Option<Language>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Display {
    /// Pixels on the panel, not the scaled size the desktop uses.
    pub width: u32,
    pub height: u32,
    pub hdr: bool,
}

impl Display {
    /// The largest standard resolution at or below the panel's shorter side, so a
    /// 3024x1964 laptop panel wants 1080p and a 3840x2160 monitor wants 2160p.
    pub const fn target(self) -> Resolution {
        let lines = if self.width < self.height { self.width } else { self.height };
        match lines {
            2160.. => Resolution::P2160,
            1080.. => Resolution::P1080,
            720.. => Resolution::P720,
            _ => Resolution::Sd,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    Macos,
    Linux,
    Other,
}

impl Os {
    pub fn of_host() -> Os {
        match std::env::consts::OS {
            "macos" => Self::Macos,
            "linux" => Self::Linux,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Arch {
    #[serde(rename = "x86_64")]
    X86_64,
    #[serde(rename = "arm64")]
    Arm64,
    /// 32-bit x86: i386 and i686.
    #[serde(rename = "x86")]
    X86,
    /// 32-bit ARM, which Debian calls armhf.
    #[serde(rename = "arm")]
    Arm,
}

impl Arch {
    pub fn of_host() -> Option<Arch> {
        match std::env::consts::ARCH {
            "x86_64" => Some(Self::X86_64),
            "aarch64" => Some(Self::Arm64),
            "x86" => Some(Self::X86),
            "arm" => Some(Self::Arm),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Arm64 => "arm64",
            Self::X86 => "x86",
            Self::Arm => "arm",
        }
    }

    /// Every architecture a name mentions, as in `debian-13.7.0-arm64-netinst.iso` or
    /// `Ubuntu 11.04 Desktop dvd amd64 (x64)`.
    pub fn named_in(name: &str) -> Vec<Arch> {
        let tokens: Vec<String> = name
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        let mut named = Vec::new();
        for (i, token) in tokens.iter().enumerate() {
            let next = tokens.get(i + 1).map_or("", String::as_str);
            let arch = match (token.as_str(), next) {
                ("x86", "64") | ("amd64" | "x64", _) => Some(Self::X86_64),
                ("arm64" | "aarch64", _) => Some(Self::Arm64),
                ("x86" | "i386" | "i686", _) => Some(Self::X86),
                ("armhf", _) => Some(Self::Arm),
                _ => None,
            };
            if let Some(arch) = arch.filter(|a| !named.contains(a)) {
                named.push(arch);
            }
        }
        named
    }
}

/// An ISO 639 language code from the system locale, such as `en` for `en-IN`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Language(String);

const MARKERS: [(&str, &str, &[&str]); 8] = [
    ("en", "English", &["english", "eng"]),
    ("es", "Spanish", &["spanish", "español", "espanol", "castellano", "latino"]),
    ("fr", "French", &["french", "truefrench", "vostfr", "vff", "vfq"]),
    ("de", "German", &["german", "deutsch", "ger"]),
    ("it", "Italian", &["italian", "ita"]),
    ("pt", "Portuguese", &["portuguese", "português", "dublado"]),
    ("ru", "Russian", &["russian", "rus"]),
    ("hi", "Hindi", &["hindi"]),
];

impl Language {
    /// From a locale such as `en-IN`, `en_US.UTF-8`, or `zh-Hans-CN`. `None` for `C`,
    /// `POSIX`, and anything else without a language code.
    pub fn from_locale(locale: &str) -> Option<Language> {
        let code = locale.split(['-', '_', '.', '@']).next()?.to_ascii_lowercase();
        let letters = (2..=3).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_lowercase());
        (letters && code != "c").then_some(Language(code))
    }

    pub fn code(&self) -> &str {
        &self.0
    }

    /// `English`, or the code for a language release names have no marker for.
    pub fn name(&self) -> &str {
        MARKERS.iter().find(|(code, ..)| *code == self.0).map_or(&self.0, |(_, name, _)| name)
    }

    /// The languages a release name marks, as `Doblada Al Español` marks Spanish. Words
    /// of the query do not count, so a search for The French Connection finds no French.
    pub fn named_in(name: &str, query_words: &[String]) -> Vec<Language> {
        let lower = name.to_lowercase();
        let words: Vec<&str> =
            lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !query_words.iter().any(|q| q == w)).collect();
        MARKERS
            .iter()
            .filter(|(_, _, markers)| words.iter().any(|w| markers.contains(w)))
            .map(|(code, ..)| Language((*code).to_string()))
            .collect()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Detected,
    /// A preference from `torrential setup`.
    Override,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
pub struct Setting<T> {
    pub value: T,
    pub source: Source,
}

/// What one search ranked for. A client sends the user's preferences, and the engine adds
/// what it detected and the room on disk before it ranks.
#[derive(Clone, PartialEq, Eq, Debug, Default, Deserialize)]
pub struct Profile {
    /// `None` until the engine finishes detecting the machine, about a second after it starts.
    #[serde(default)]
    pub detected: Option<Machine>,
    #[serde(default)]
    pub preferences: Preferences,
    /// Bytes a download may take in the download folder: the free space less the reserve
    /// `get` keeps. `None` when unknown.
    #[serde(default)]
    pub room: Option<u64>,
}

impl Profile {
    /// The resolution ranking aims for: the preferred screen's, else the detected display's.
    pub fn screen(&self) -> Option<Setting<Resolution>> {
        match (self.preferences.screen, self.display()) {
            (Some(screen), _) => Some(Setting { value: screen.target(), source: Source::Override }),
            (None, Some(display)) => Some(Setting { value: display.target(), source: Source::Detected }),
            (None, None) => None,
        }
    }

    /// An explicit playback preference wins. Otherwise the detected display chooses
    /// HEVC with or without HDR. A screen override leaves playback unknown.
    pub fn playback(&self) -> Option<Setting<Playback>> {
        if let Some(playback) = self.preferences.playback {
            return Some(Setting { value: playback, source: Source::Override });
        }
        let display = self.display().filter(|_| self.preferences.screen.is_none())?;
        let value = if display.hdr { Playback::HevcHdr } else { Playback::Hevc };
        Some(Setting { value, source: Source::Detected })
    }

    fn display(&self) -> Option<Display> {
        self.detected.as_ref().and_then(|m| m.display)
    }

    fn arch(&self) -> Option<Arch> {
        self.detected.as_ref().and_then(|m| m.arch)
    }

    fn language(&self) -> Option<&Language> {
        self.detected.as_ref().and_then(|m| m.language.as_ref())
    }

    /// False only for a release known to be larger than `max_size` or the room on disk, or
    /// known to need a codec or HDR the player may not handle. Unknown values fit.
    pub fn fits(&self, size: Option<u64>, quality: Quality) -> bool {
        let within = |cap: Option<u64>| size.zip(cap).is_none_or(|(size, cap)| size <= cap);
        within(self.preferences.max_size)
            && within(self.room)
            && self.playback().is_none_or(|playback| playback.value.plays(quality))
    }

    /// Lower is closer to the screen's resolution: the same, then one step lower, then one
    /// higher, then two lower, and so on. A bigger picture than the screen shows wastes
    /// bytes, a smaller one loses detail. Without a screen every release ties at 0. With one,
    /// an unknown resolution comes after every known one, since a name without a tag gives
    /// no sign that it suits the screen.
    pub fn closeness(&self, resolution: Option<Resolution>) -> u8 {
        let step = |r: Resolution| match r {
            Resolution::Sd => 0i8,
            Resolution::P720 => 1,
            Resolution::P1080 => 2,
            Resolution::P2160 => 3,
        };
        match (self.screen(), resolution) {
            (None, _) => 0,
            (Some(screen), Some(resolution)) => {
                let steps = step(resolution) - step(screen.value);
                if steps < 0 { steps.unsigned_abs() * 2 - 1 } else { steps.unsigned_abs() * 2 }
            }
            (Some(_), None) => u8::MAX,
        }
    }

    /// 0 for a name that mentions this machine's architecture, and 1 for any other. A name
    /// for another architecture ranks no lower than one without, so on an arm64 Mac a search
    /// for Ubuntu still leads with its amd64 ISOs, not with TV episodes that mention Ubuntu.
    pub fn arch_rank(&self, named: &[Arch]) -> u8 {
        u8::from(self.arch().is_some_and(|host| !named.contains(&host)))
    }

    /// 0 for a name that marks the system language, 1 for one in the original language, and
    /// 2 for one that marks only others. The original is usually English, so a name in
    /// English or without a marker is the original, and for English it ranks first.
    pub fn language_rank(&self, named: &[Language]) -> u8 {
        let Some(own) = self.language() else { return 0 };
        let original = named.is_empty() || named.iter().any(|l| l.code() == "en");
        if named.contains(own) || (original && own.code() == "en") {
            0
        } else if original {
            1
        } else {
            2
        }
    }

    /// `this Mac: 1080p screen, HDR, HEVC, arm64, English`.
    pub fn describe(&self) -> String {
        let screen = match self.screen() {
            Some(screen) => Some(format!("{} screen", screen_label(screen.value))),
            None => self.detected.as_ref().map(|_| "no screen".to_string()),
        };
        let parts: Vec<String> = [
            screen,
            self.playback().map(|p| playback_phrase(p.value).to_string()),
            self.preferences.max_size.map(|max| format!("up to {}", format_limit(max))),
            self.arch().map(|a| a.as_str().to_string()),
            self.language().map(|l| l.name().to_string()),
        ]
        .into_iter()
        .flatten()
        .collect();
        self.labeled(&parts)
    }

    /// `this Mac: arm64`, for software, where only the architecture matters.
    pub fn describe_arch(&self) -> String {
        self.labeled(&self.arch().map(|a| a.as_str().to_string()).into_iter().collect::<Vec<_>>())
    }

    fn labeled(&self, parts: &[String]) -> String {
        let machine = match &self.detected {
            Some(Machine { os: Os::Macos, .. }) => "this Mac",
            Some(_) => "this computer",
            None => "this machine, not detected yet",
        };
        if parts.is_empty() { machine.to_string() } else { format!("{machine}: {}", parts.join(", ")) }
    }

    /// `screen tv-4k, max_size 8GB`, or `None` when nothing overrides detection.
    pub fn overrides(&self) -> Option<String> {
        let p = &self.preferences;
        let parts: Vec<String> = [
            p.screen.map(|s| format!("screen {}", s.as_str())),
            p.max_size.map(|max| format!("max_size {}", size_text(max))),
            p.playback.map(|p| format!("playback {}", p.as_str())),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

const fn playback_phrase(playback: Playback) -> &'static str {
    match playback {
        Playback::HevcHdr => "HDR, HEVC",
        Playback::Hevc => "no HDR, HEVC",
        Playback::H264 => "no HDR, H.264 only",
    }
}

const fn screen_label(resolution: Resolution) -> &'static str {
    match resolution {
        Resolution::Sd => "SD",
        other => other.as_str(),
    }
}

/// The values ranking used, each with where it came from, then what was detected and the
/// preferences. Reading it back keeps only the last two and the room.
impl Serialize for Profile {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Fields<'a> {
            screen: Option<Setting<Resolution>>,
            playback: Option<Setting<Playback>>,
            max_size: Option<u64>,
            room: Option<u64>,
            arch: Option<Arch>,
            language: Option<&'a Language>,
            detected: &'a Option<Machine>,
            preferences: &'a Preferences,
        }
        Fields {
            screen: self.screen(),
            playback: self.playback(),
            max_size: self.preferences.max_size,
            room: self.room,
            arch: self.arch(),
            language: self.language(),
            detected: &self.detected,
            preferences: &self.preferences,
        }
        .serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preferences::Screen;

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
        Profile { detected, preferences, room: None }
    }

    #[test]
    fn a_display_wants_the_largest_standard_resolution_its_panel_shows() {
        let target = |width, height| Display { width, height, hdr: false }.target().as_str();
        let panels = [(3024, 1964), (3840, 2160), (5120, 2880), (2560, 1440), (1920, 1080), (1366, 768), (1024, 600)];
        assert_eq!(panels.map(|(w, h)| target(w, h)), ["1080p", "2160p", "2160p", "1080p", "1080p", "720p", "sd"]);
        assert_eq!(target(2160, 3840), "2160p", "a portrait panel counts its shorter side");
    }

    #[test]
    fn preferences_override_what_was_detected() {
        let tv = Preferences { screen: Some(Screen::Tv4k), ..Preferences::default() };
        let h264 = Preferences { playback: Some(Playback::H264), ..Preferences::default() };
        let read = |p: &Profile| (p.screen().map(|s| (s.value, s.source)), p.playback().map(|p| (p.value, p.source)));

        assert_eq!(
            read(&profile(Some(mac()), Preferences::default())),
            (Some((Resolution::P1080, Source::Detected)), Some((Playback::HevcHdr, Source::Detected)))
        );
        assert_eq!(read(&profile(Some(mac()), tv)), (Some((Resolution::P2160, Source::Override)), None));
        assert_eq!(
            read(&profile(Some(mac()), h264)),
            (Some((Resolution::P1080, Source::Detected)), Some((Playback::H264, Source::Override)))
        );
        assert_eq!(read(&profile(Some(server()), Preferences::default())), (None, None));
        assert_eq!(read(&profile(None, tv)), (Some((Resolution::P2160, Source::Override)), None));
    }

    #[test]
    fn an_sdr_display_ranks_hdr_as_unplayable_and_a_full_disk_ranks_big_files_out() {
        let sdr = Machine { display: Some(Display { width: 1920, height: 1080, hdr: false }), ..mac() };
        let fits = |p: &Profile, size, name| p.fits(Some(size), Quality::parse(name));
        let on_sdr = profile(Some(sdr), Preferences::default());
        let full = Profile { room: Some(4_000_000_000), ..profile(Some(mac()), Preferences::default()) };

        assert_eq!([fits(&on_sdr, 1, "Film 2160p HDR x265"), fits(&on_sdr, 1, "Film 2160p x265")], [false, true]);
        assert_eq!([fits(&full, 4_000_000_000, "Film"), fits(&full, 4_000_000_001, "Film")], [true, false]);
        assert!(full.fits(None, Quality::default()), "an unknown size fits");
    }

    #[test]
    fn closeness_runs_same_then_lower_then_higher_with_unknown_last() {
        use Resolution::{P720, P1080, P2160, Sd};
        let order = |screen| {
            let p = profile(Some(mac()), Preferences { screen, ..Preferences::default() });
            [Some(P2160), Some(P1080), Some(P720), Some(Sd), None].map(|r| p.closeness(r))
        };
        assert_eq!(order(None), [2, 0, 1, 3, u8::MAX], "this Mac's 1080p panel");
        assert_eq!(order(Some(Screen::Tv4k)), [0, 1, 3, 5, u8::MAX]);
        assert_eq!(order(Some(Screen::Phone)), [4, 2, 0, 1, u8::MAX]);
        let headless = profile(Some(server()), Preferences::default());
        assert_eq!([Some(P2160), Some(Sd), None].map(|r| headless.closeness(r)), [0, 0, 0]);
    }

    #[test]
    fn reads_architectures_from_recorded_names() {
        let cases = [
            ("debian-13.7.0-arm64-netinst.iso", vec![Arch::Arm64]),
            ("debian-13.7.0-amd64-netinst.iso", vec![Arch::X86_64]),
            ("archlinux-2026.10.01-x86_64.iso", vec![Arch::X86_64]),
            ("Ubuntu 11.04 Desktop dvd amd64 (x64) .iso", vec![Arch::X86_64]),
            ("Ubuntu 11.04 Desktop dvd -i386 (x86) .iso", vec![Arch::X86]),
            ("Jorgensen E. x86-64 Assembly Language Programming...Ubuntu 2020", vec![Arch::X86_64]),
            ("[4LS]ks_a1_[linux-x86][FFD2F7FD].tar.bz2", vec![Arch::X86]),
            ("ubuntu-9.04-desktop-i686 aarch64 armhf", vec![Arch::X86, Arch::Arm64, Arch::Arm]),
            ("Night of the Living Dead 1968 1080p x264", vec![]),
        ];
        let read: Vec<(&str, Vec<Arch>)> = cases.iter().map(|(name, _)| (*name, Arch::named_in(name))).collect();
        assert_eq!(read, cases);
    }

    #[test]
    fn the_host_architecture_ranks_first() {
        let arm = profile(Some(mac()), Preferences::default());
        let ranks = |p: &Profile| {
            [&[Arch::Arm64][..], &[], &[Arch::X86_64], &[Arch::X86_64, Arch::Arm64]].map(|n| p.arch_rank(n))
        };
        assert_eq!(ranks(&arm), [0, 1, 1, 0]);
        assert_eq!(ranks(&profile(None, Preferences::default())), [0, 0, 0, 0]);
    }

    #[test]
    fn reads_language_markers_from_recorded_names() {
        let marked = |name: &str, query: &str| -> Vec<String> {
            let words: Vec<String> = query.split(' ').map(str::to_string).collect();
            Language::named_in(name, &words).iter().map(|l| l.code().to_string()).collect()
        };
        let film = "night of the living dead 1968";
        assert_eq!(marked("Night Of The Living Dead ( 1968) Doblada Al Español FULL HD", film), ["es"]);
        assert_eq!(marked("Night Of The Living Dead ( 1968) English FULL HD", film), ["en"]);
        assert_eq!(marked("Night Of The Living Dead (with spanish subtitles)", film), ["es"]);
        assert_eq!(marked("Night of the Living Dead (1968) [colorised, 4k, 60FPS]", film), Vec::<String>::new());
        assert_eq!(marked("Big Buck Bunny (2008) German Blu-ray Disc", "big buck bunny"), ["de"]);
        assert_eq!(marked("Ubuntu Facile - Aprile 2015, [Pdf - Ita] [TNTVillage]", "ubuntu"), ["it"]);
        assert_eq!(marked("Movie 2024 VOSTFR 1080p", "movie"), ["fr"]);
        assert_eq!(marked("Movie 2024 iTALiAN 1080p", "movie"), ["it"]);
        assert_eq!(marked("Movie 2024 Dual Latino 1080p", "movie"), ["es"]);
        assert_eq!(marked("Movie.2024.1080p.WEB-DL.Rus.Eng", "movie"), ["en", "ru"]);
        assert_eq!(marked("The French Connection 1971 1080p", "the french connection"), Vec::<String>::new());
    }

    #[test]
    fn the_system_language_ranks_first_and_another_last() {
        let with_locale = |locale| {
            profile(Some(Machine { language: Language::from_locale(locale), ..mac() }), Preferences::default())
        };
        let es = Language::from_locale("es").unwrap();
        let en = Language::from_locale("en").unwrap();
        let names = [vec![], vec![en.clone()], vec![es.clone()], vec![es, en]];
        let ranks = |p: Profile| names.clone().map(|n| p.language_rank(&n));
        assert_eq!(ranks(with_locale("en-IN")), [0, 0, 2, 0]);
        assert_eq!(ranks(with_locale("es_MX.UTF-8")), [1, 1, 0, 0]);
        assert_eq!(ranks(with_locale("ja-JP")), [1, 1, 2, 1]);
    }

    #[test]
    fn reads_the_language_of_a_locale() {
        let read = |locale| Language::from_locale(locale).map(|l| (l.code().to_string(), l.name().to_string()));
        assert_eq!(read("en-IN"), Some(("en".into(), "English".into())));
        assert_eq!(read("fr_FR.UTF-8"), Some(("fr".into(), "French".into())));
        assert_eq!(read("zh-Hans-CN"), Some(("zh".into(), "zh".into())));
        assert_eq!(read("C.UTF-8"), None);
        assert_eq!(read("POSIX"), None);
    }

    #[test]
    fn describes_what_ranking_used_and_what_overrode_it() {
        let tv = Preferences { screen: Some(Screen::Tv4k), max_size: Some(8_000_000_000), ..Preferences::default() };
        let line = |p: Profile| (p.describe(), p.overrides());
        assert_eq!(
            line(profile(Some(mac()), Preferences::default())),
            ("this Mac: 1080p screen, HDR, HEVC, arm64, English".into(), None)
        );
        assert_eq!(
            line(profile(Some(mac()), tv)),
            ("this Mac: 2160p screen, up to 8 GB, arm64, English".into(), Some("screen tv-4k, max_size 8GB".into()))
        );
        assert_eq!(
            line(profile(Some(server()), Preferences::default())),
            ("this computer: no screen, x86_64".into(), None)
        );
        assert_eq!(line(profile(None, Preferences::default())), ("this machine, not detected yet".into(), None));
        assert_eq!(profile(Some(mac()), tv).describe_arch(), "this Mac: arm64");
    }

    #[test]
    fn json_carries_each_value_with_its_source_and_reads_back() {
        let tv = Profile {
            room: Some(1 << 40),
            ..profile(Some(mac()), Preferences { screen: Some(Screen::Tv4k), ..Preferences::default() })
        };
        let json = serde_json::to_value(&tv).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "screen": {"value": "2160p", "source": "override"},
                "playback": null,
                "max_size": null,
                "room": 1u64 << 40,
                "arch": "arm64",
                "language": "en",
                "detected": {
                    "os": "macos",
                    "display": {"width": 3024, "height": 1964, "hdr": true},
                    "arch": "arm64",
                    "language": "en"
                },
                "preferences": {"screen": "tv-4k", "max_size": null, "playback": null}
            })
        );
        assert_eq!(serde_json::from_value::<Profile>(json).unwrap(), tv);
    }
}
