use serde::{Deserialize, Serialize, Serializer};

use crate::name;
use crate::preferences::{Playback, Preferences, Screen};
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
        panel_target(self.width, self.height)
    }
}

const fn panel_target(width: u32, height: u32) -> Resolution {
    let lines = if width < height { width } else { height };
    match lines {
        2160.. => Resolution::P2160,
        1080.. => Resolution::P1080,
        720.. => Resolution::P720,
        _ => Resolution::Sd,
    }
}

/// The device that pressed play, such as a phone or a TV: the resolution that suits its
/// screen and what its player decodes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Viewer {
    pub screen: Resolution,
    pub playback: Playback,
}

/// CSS pixels across the shorter side below which a screen is a phone's. Phones measure 320
/// to 480, and the smallest tablets and TV browsers 540 or more.
const PHONE_BELOW: f64 = 500.0;

impl Viewer {
    /// A screen a browser measured as `width` by `height` device pixels, with `scale` of them
    /// to a CSS pixel. A CSS pixel looks about the same size on any screen at its usual
    /// distance, so the CSS size tells a phone from a TV with as many device pixels. A phone
    /// wants what the `phone` screen preference wants, and any other screen what a display
    /// with as many pixels wants.
    pub fn measured(width: u32, height: u32, scale: f64, playback: Playback) -> Viewer {
        let phone = f64::from(width.min(height)) / scale < PHONE_BELOW;
        let screen = if phone { Screen::Phone.target() } else { panel_target(width, height) };
        Viewer { screen, playback }
    }
}

/// What a title plays in, which decides whose screen and formats its sources are ranked for.
/// config.yaml's `screen` and `playback` describe this machine's display and the player it opens,
/// so they rank for nothing else.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Player {
    /// The player `torrential play` and the terminal UI open on this machine, which searches rank
    /// for too: config.yaml's `screen` and `playback`, else what the engine detected.
    #[default]
    External,
    /// A browser on this machine. It shows this machine's display, or the screen in config.yaml,
    /// and plays what it decodes.
    Browser { playback: Playback },
    /// A browser on another device, such as a phone or a TV: the screen and formats it reported.
    Device(Viewer),
}

impl Player {
    /// The screen ranking aims for, when the player reported the one that decides.
    const fn screen(self) -> Option<Resolution> {
        match self {
            Self::Device(viewer) => Some(viewer.screen),
            Self::External | Self::Browser { .. } => None,
        }
    }

    /// What the player decodes, when it reported that.
    const fn decodes(self) -> Option<Playback> {
        match self {
            Self::Browser { playback } | Self::Device(Viewer { playback, .. }) => Some(playback),
            Self::External => None,
        }
    }

    /// Whether the player streams the file as it downloads, so a bitrate its screen cannot show
    /// only makes the download take longer.
    pub const fn streams(self) -> bool {
        !matches!(self, Self::External)
    }

    /// Whether the player decodes a video of `quality`, as far as its name tells. One that did not
    /// say what it decodes plays any.
    pub const fn plays(self, quality: Quality) -> bool {
        match self.decodes() {
            Some(playback) => playback.plays(quality),
            None => true,
        }
    }

    /// Whether a download already on this machine, of `quality`, plays before anything a search
    /// finds: in a browser that decodes it or that `ffmpeg` converts it for, and in the player this
    /// machine opens always, as before. Size and bitrate do not count, since the file costs no
    /// download.
    pub const fn reuses(self, quality: Quality, ffmpeg: bool) -> bool {
        ffmpeg || self.plays(quality)
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

crate::words! {
    pub enum Arch {
        X86_64 = "x86_64",
        Arm64 = "arm64",
        /// 32-bit x86: i386 and i686.
        X86 = "x86",
        /// 32-bit ARM, which Debian calls armhf.
        Arm = "arm",
    }
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

    /// Every architecture a name mentions, as in `debian-13.7.0-arm64-netinst.iso` or
    /// `Ubuntu 11.04 Desktop dvd amd64 (x64)`.
    pub fn named_in(name: &str) -> Vec<Arch> {
        let tokens = name::tags(name);
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

const MARKERS: [(&str, &str, &[&str]); 9] = [
    ("en", "English", &["english", "eng"]),
    ("es", "Spanish", &["spanish", "español", "espanol", "castellano", "latino", "doblada", "doblado"]),
    ("fr", "French", &["french", "truefrench", "vostfr", "vff", "vfq"]),
    ("de", "German", &["german", "deutsch", "ger"]),
    ("it", "Italian", &["italian", "ita"]),
    ("pt", "Portuguese", &["portuguese", "português", "dublado"]),
    ("ru", "Russian", &["russian", "rus"]),
    ("hi", "Hindi", &["hindi"]),
    ("sv", "Swedish", &["swedish", "swesub"]),
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
    /// of the query do not count, so a search for The French Connection finds no French, and
    /// neither do subtitles, as in `with spanish subtitles`.
    pub fn named_in(name: &str, query_words: &[String]) -> Vec<Language> {
        const SUBTITLES: [&str; 5] = ["sub", "subs", "subbed", "subtitle", "subtitles"];
        let lower = name.to_lowercase();
        let all: Vec<&str> = name::words(&lower).collect();
        let words: Vec<&str> = all
            .iter()
            .enumerate()
            .filter(|(i, w)| {
                !query_words.iter().any(|q| q == *w) && !all.get(i + 1).is_some_and(|next| SUBTITLES.contains(next))
            })
            .map(|(_, w)| *w)
            .collect();
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
    /// What the browser that pressed play reported.
    Viewer,
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
    #[serde(default)]
    pub player: Player,
}

impl Profile {
    /// The resolution ranking aims for: another device's screen, else the preferred screen's,
    /// else the detected display's.
    pub fn screen(&self) -> Option<Setting<Resolution>> {
        if let Some(screen) = self.player.screen() {
            return Some(Setting { value: screen, source: Source::Viewer });
        }
        match (self.preferences.screen, self.display()) {
            (Some(screen), _) => Some(Setting { value: screen.target(), source: Source::Override }),
            (None, Some(display)) => Some(Setting { value: display.target(), source: Source::Detected }),
            (None, None) => None,
        }
    }

    /// What a browser decodes. For the player this machine opens, an explicit playback
    /// preference wins, and otherwise the detected display chooses HEVC with or without HDR. A
    /// screen override leaves its playback unknown.
    pub fn playback(&self) -> Option<Setting<Playback>> {
        if let Some(playback) = self.player.decodes() {
            return Some(Setting { value: playback, source: Source::Viewer });
        }
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

    pub fn arch(&self) -> Option<Arch> {
        self.detected.as_ref().and_then(|m| m.arch)
    }

    pub fn language(&self) -> Option<&Language> {
        self.detected.as_ref().and_then(|m| m.language.as_ref())
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

/// The values ranking used, each with where it came from, then what was detected, the
/// preferences, and the player when it is a browser. Reading it back keeps only the last three
/// and the room.
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
            #[serde(skip_serializing_if = "Option::is_none")]
            player: Option<Player>,
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
            player: (self.player != Player::External).then_some(self.player),
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
    fn a_screen_under_500_css_pixels_across_is_a_phone_and_any_other_wants_what_its_pixels_show() {
        let screen = |width, height, scale| Viewer::measured(width, height, scale, Playback::Hevc).screen.as_str();
        assert_eq!(screen(1170, 2532, 3.0), "720p", "an iPhone");
        assert_eq!(screen(2400, 1080, 2.625), "720p", "an Android phone on its side");
        assert_eq!(screen(1640, 2360, 2.0), "1080p", "an iPad");
        assert_eq!(screen(3024, 1964, 2.0), "1080p", "a MacBook");
        assert_eq!(screen(1920, 1080, 2.0), "1080p", "a TV browser that lays out 960x540");
        assert_eq!(screen(1920, 1080, 1.0), "1080p", "a 1080p TV");
        assert_eq!(screen(3840, 2160, 2.0), "2160p", "a 4K TV browser that lays out 1920x1080");
        assert_eq!(screen(3840, 2160, 1.0), "2160p", "a 4K TV");
        assert_eq!(screen(1280, 720, 1.0), "720p", "a 720p TV");
    }

    #[test]
    fn another_device_ranks_for_its_own_screen_a_browser_for_what_it_decodes_and_config_for_this_machine() {
        let tv_h264 =
            Preferences { screen: Some(Screen::Tv4k), playback: Some(Playback::H264), ..Preferences::default() };
        let read = |p: Profile| (p.screen().map(|s| (s.value, s.source)), p.playback().map(|p| (p.value, p.source)));
        let browser = |playback| Player::Browser { playback };

        assert_eq!(
            read(playing_in(Player::External, tv_h264)),
            (Some((Resolution::P2160, Source::Override)), Some((Playback::H264, Source::Override)))
        );
        assert_eq!(
            read(playing_in(browser(Playback::Hevc), tv_h264)),
            (Some((Resolution::P2160, Source::Override)), Some((Playback::Hevc, Source::Viewer))),
            "a browser here shows the screen in config.yaml"
        );
        assert_eq!(
            read(playing_in(browser(Playback::H264), Preferences::default())),
            (Some((Resolution::P1080, Source::Detected)), Some((Playback::H264, Source::Viewer))),
            "a browser here shows the detected display"
        );
        assert_eq!(
            read(watching_on(phone(), tv_h264)),
            (Some((Resolution::P720, Source::Viewer)), Some((Playback::Hevc, Source::Viewer)))
        );
        assert_eq!(
            read(watching_on(tv_4k(), Preferences::default())),
            (Some((Resolution::P2160, Source::Viewer)), Some((Playback::HevcHdr, Source::Viewer)))
        );
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
    fn reads_language_markers_from_recorded_names() {
        let marked = |name: &str, query: &str| -> Vec<String> {
            let words: Vec<String> = query.split(' ').map(str::to_string).collect();
            Language::named_in(name, &words).iter().map(|l| l.code().to_string()).collect()
        };
        let film = "night of the living dead 1968";
        assert_eq!(marked("Night Of The Living Dead ( 1968) Doblada Al Español FULL HD", film), ["es"]);
        assert_eq!(marked("Night Of The Living Dead ( 1968) English FULL HD", film), ["en"]);
        assert_eq!(marked("Night Of The Living Dead (with spanish subtitles)", film), Vec::<String>::new());
        assert_eq!(marked("Movie 2024 1080p English Subs", "movie"), Vec::<String>::new());
        assert_eq!(marked("Night of the Living Dead (1968) [colorised, 4k, 60FPS]", film), Vec::<String>::new());
        assert_eq!(marked("Big Buck Bunny (2008) German Blu-ray Disc", "big buck bunny"), ["de"]);
        assert_eq!(marked("Ubuntu Facile - Aprile 2015, [Pdf - Ita] [TNTVillage]", "ubuntu"), ["it"]);
        assert_eq!(marked("Movie 2024 VOSTFR 1080p", "movie"), ["fr"]);
        assert_eq!(marked("Movie 2024 iTALiAN 1080p", "movie"), ["it"]);
        assert_eq!(marked("Movie 2024 Dual Latino 1080p", "movie"), ["es"]);
        assert_eq!(marked("Movie.2024.1080p.WEB-DL.Rus.Eng", "movie"), ["en", "ru"]);
        assert_eq!(marked("The French Connection 1971 1080p", "the french connection"), Vec::<String>::new());
        assert_eq!(marked("Movie 2024 Doblada 1080p", "movie"), ["es"]);
        assert_eq!(marked("Sintel.2010.720p.SWESUB.x264-SMS", "sintel 2010"), ["sv"]);
        assert_eq!(marked("Sintel, O Filme 2010 HD Dublado PT BR", "sintel 2010"), ["pt"]);
        assert_eq!(marked("Night of the Living Dead (1968) 1080p Ita Eng MIRCrew", film), ["en", "it"]);
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
        let on_phone = watching_on(phone(), Preferences::default());
        let json = serde_json::to_value(&on_phone).unwrap();
        assert_eq!(
            (&json["screen"], &json["playback"], &json["player"]),
            (
                &serde_json::json!({"value": "720p", "source": "viewer"}),
                &serde_json::json!({"value": "hevc", "source": "viewer"}),
                &serde_json::json!({"kind": "device", "screen": "720p", "playback": "hevc"})
            )
        );
        assert_eq!(serde_json::from_value::<Profile>(json).unwrap(), on_phone);
    }
}
